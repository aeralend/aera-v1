//! Liquidation: the health gate, the close factor, the 8% bonus, and the
//! escalation to a full close below HF 0.95.

mod common;

use common::*;
use solana_keypair::Keypair;

/// A borrower with 1,000 bCOOK at 1.0 COOK who has drawn close to their limit.
///
///   effective collateral = 1,000 * 1.00 * 0.95 = 950
///   borrow limit         = 950 * 0.55          = 522.5
///   liquidation line     = 950 * 0.65          = 617.5
fn stressed_borrower(
    bcook_price_after: u64,
) -> (Env, ReserveHandle, ReserveHandle, Keypair, Pubkey) {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(522), &[&cook, &bcook])
        .unwrap();

    // bCOOK falls. Stays under the 25% breaker threshold so the move is a
    // normal drawdown rather than a halt.
    env.set_price(bcook.mint, px(bcook_price_after));
    (env, cook, bcook, borrower, obligation)
}

fn refresh(env: &mut Env, cook: &ReserveHandle, bcook: &ReserveHandle, obligation: Pubkey) {
    // A no-op repay is the cheapest way to force a refresh through the harness.
    let instructions = {
        let mut ixs = env.accrue_all_ixs(&[cook, bcook]);
        ixs.push(env.refresh_obligation_ix(obligation));
        ixs
    };
    let admin = env.admin.insecure_clone();
    solana_kite::send_transaction_from_instructions(
        &mut env.svm,
        instructions,
        &[&admin],
        &admin.pubkey(),
    )
    .unwrap();
}

#[test]
fn healthy_positions_cannot_be_liquidated() {
    // 0.90 keeps the position healthy: 1,000 * 0.90 * 0.95 * 0.65 = 555.75
    // against 522 of debt, so HF is about 1.06.
    let (mut env, cook, bcook, _borrower, obligation) = stressed_borrower(900);
    refresh(&mut env, &cook, &bcook, obligation);

    let state = env.read_obligation(obligation);
    assert!(!state.is_liquidatable());
    let hf = state.health_factor_bps().unwrap().unwrap();
    assert!(
        (10_000..11_000).contains(&hf),
        "HF {hf} should be just over 1"
    );

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(1_000));
    assert_error(
        env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(100)),
        "ObligationHealthy",
    );
}

/// Between HF 0.95 and 1.0 the close factor caps a liquidation at half the debt.
#[test]
fn close_factor_caps_the_repayment() {
    // 0.82 -> line = 1,000 * 0.82 * 0.95 * 0.65 = 506.35 against 522 of debt,
    // so HF is about 0.970: liquidatable, but not deeply.
    let (mut env, cook, bcook, _borrower, obligation) = stressed_borrower(820);
    refresh(&mut env, &cook, &bcook, obligation);

    let state = env.read_obligation(obligation);
    assert!(state.is_liquidatable());
    let hf = state.health_factor_bps().unwrap().unwrap();
    assert!(
        (9_500..10_000).contains(&hf),
        "HF {hf} should sit between 0.95 and 1.0"
    );

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(2_000));
    let before = env.balance(&ata(&liquidator.pubkey(), &cook.mint));

    // Ask to repay everything; the close factor should allow only half.
    env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(522))
        .unwrap();

    let spent = before - env.balance(&ata(&liquidator.pubkey(), &cook.mint));
    let half = tokens(261);
    assert!(
        spent.abs_diff(half) < tokens(1),
        "expected ~{half} repaid (50% close factor), got {spent}"
    );
}

/// Below HF 0.95 the whole position may be closed in one go.
#[test]
fn deep_underwater_positions_close_fully() {
    // 0.75 -> line = 1,000 * 0.75 * 0.95 * 0.65 = 463.1 against 522, HF ~0.887.
    let (mut env, cook, bcook, _borrower, obligation) = stressed_borrower(750);
    refresh(&mut env, &cook, &bcook, obligation);

    let state = env.read_obligation(obligation);
    let hf = state.health_factor_bps().unwrap().unwrap();
    assert!(hf < 9_500, "HF {hf} should be below 0.95");

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(2_000));
    let before = env.balance(&ata(&liquidator.pubkey(), &cook.mint));

    env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(522))
        .unwrap();

    let spent = before - env.balance(&ata(&liquidator.pubkey(), &cook.mint));
    assert!(
        spent.abs_diff(tokens(522)) < tokens(1),
        "expected the full 522 repaid below HF 0.95, got {spent}"
    );

    // The debt is gone.
    refresh(&mut env, &cook, &bcook, obligation);
    assert_eq!(env.read_obligation(obligation).borrowed_value, 0);
}

/// The liquidator is paid the repaid value plus 8%, priced in the collateral.
#[test]
fn liquidator_receives_the_eight_percent_bonus() {
    let (mut env, cook, bcook, _borrower, obligation) = stressed_borrower(820);
    refresh(&mut env, &cook, &bcook, obligation);

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(2_000));
    // Share mints are Token-2022, so the collateral ATA lives under that
    // program, not the legacy one the liquidity accounts use.
    let collateral_account = env.ensure_share_ata(&liquidator, bcook.share_mint);
    assert_eq!(env.balance(&collateral_account), 0);

    let cook_before = env.balance(&ata(&liquidator.pubkey(), &cook.mint));
    env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(100))
        .unwrap();

    let repaid = cook_before - env.balance(&ata(&liquidator.pubkey(), &cook.mint));
    assert_eq!(repaid, tokens(100));

    // Seized value = 100 COOK * 1.08 = 108 COOK, priced at 0.82 COOK per bCOOK
    // and 1:1 into shares (the bCOOK reserve is never borrowed, so its index
    // never moves): 108 / 0.82 = 131.7 shares.
    let seized = env.balance(&collateral_account);
    let expected = tokens(108) * 1_000 / 820;
    assert!(
        seized.abs_diff(expected) < tokens(1),
        "expected ~{expected} bCOOK shares seized, got {seized}"
    );

    // The bonus is real: what they took out is worth more than what they put in.
    let seized_value_in_cook = seized * 820 / 1_000;
    assert!(
        seized_value_in_cook > repaid,
        "seizing {seized_value_in_cook} COOK of value for {repaid} is not a bonus"
    );
}

/// Liquidation keeps working while the protocol is paused: it is how the pool
/// avoids bad debt, and it is needed most in the conditions that trip a pause.
#[test]
fn liquidation_survives_a_pause() {
    let (mut env, cook, bcook, _borrower, obligation) = stressed_borrower(820);
    refresh(&mut env, &cook, &bcook, obligation);

    env.pause_all();

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(2_000));
    env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(100))
        .unwrap();
}
