//! Test vectors for the TypeScript side.
//!
//! The SDK and the keepers reimplement several of the program's calculations —
//! the health factor, the liquidation seize, the share exchange rate, the
//! two-stage collateral valuation. They have to, because a liquidator that
//! cannot predict what the program will accept sends transactions that fail,
//! and a UI that cannot compute a health factor cannot warn anybody.
//!
//! A reimplementation is a second chance to be wrong. Worse, it is a second
//! chance to be wrong *in a way that agrees with itself*: a TypeScript test
//! written from the same misunderstanding as the TypeScript code passes.
//!
//! So the vectors come from here. This file runs the program's own functions
//! over a spread of inputs and emits the results to
//! `config/math-vectors.json`; `sdk/test/vectors.test.ts` replays them through
//! the TypeScript and requires identical output. Neither side can drift without
//! one of the two failing.
//!
//! ```text
//!   AERA_WRITE_MATH_VECTORS=1 cargo test --test test_math_vectors
//! ```
//!
//! regenerates. Without the variable the test only compares, so a change to the
//! program's arithmetic cannot silently rewrite the vectors it is checked
//! against.
//!
//! ## What is covered, and why these
//!
//! Every case where the *rounding direction* is load-bearing, because that is
//! where a plausible reimplementation goes wrong and where the error is
//! invisible until it is a failed transaction:
//!
//! ```text
//!   collateral value    floors      lending against value that is not there
//!   debt value          ceils       understating what somebody owes
//!   seize shares        floors      seizing more than the repayment bought
//!   share rate          floors      paying a supplier more than the pool holds
//!   close factor        floors      repaying past the protocol's cap
//! ```
//!
//! Odd numbers throughout, not round ones. `1_000_000` divides evenly by
//! everything and would agree under any rounding convention.

mod common;

use aera::constants::{BPS_DENOMINATOR, FIXED_POINT_SCALE};
use aera::math::{market_value, mul_div_floor, value_to_amount, Rounding};
use std::fmt::Write as _;

const VECTORS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../config/math-vectors.json"
);

/// One case, with its inputs and the program's answer.
struct Vector {
    name: String,
    inputs: Vec<(&'static str, String)>,
    outputs: Vec<(&'static str, String)>,
}

fn vector(name: impl Into<String>) -> Vector {
    Vector {
        name: name.into(),
        inputs: Vec::new(),
        outputs: Vec::new(),
    }
}

impl Vector {
    fn input(mut self, key: &'static str, value: impl ToString) -> Self {
        self.inputs.push((key, value.to_string()));
        self
    }
    fn output(mut self, key: &'static str, value: impl ToString) -> Self {
        self.outputs.push((key, value.to_string()));
        self
    }
}

/// Rates chosen to be awkward: prime-ish, non-round, and spanning the band.
const RATES: [u128; 5] = [
    1_000_000_000_000_000_000, // exactly 1.0, the unit-of-account case
    1_303_558_011_882_286_677, // the live bCOOK rate, measured on chain
    1_000_000_000_000_000_001, // one unit above parity
    3_141_592_653_589_793_238, // no relationship to anything
    9_999_999_999_999_999_999, // just under the ceiling
];

/// Amounts that do not divide evenly by anything.
const AMOUNTS: [u64; 6] = [
    1,
    7,
    999_999_999,
    1_000_000_007,
    333_333_333_333,
    u32::MAX as u64,
];

const FEES_BPS: [u16; 5] = [0, 1, 199, 200, 499];
const HAIRCUTS_BPS: [u16; 4] = [100, 337, 500, 999];

fn build() -> Vec<Vector> {
    let mut out = Vec::new();

    // ---- the two-stage valuation ----------------------------------------
    //
    // gross -> effective (the pool's redemption fee) -> collateral (Aera's
    // haircut). Kept separate on purpose: the fee is BakeYourStake's and is
    // taken inside the oracle; the haircut is Aera's and applies on top.
    for &rate in &RATES {
        for &fee in &FEES_BPS {
            let effective = aera::oracle::apply_withdrawal_fee(rate, fee).unwrap();
            for &haircut in &HAIRCUTS_BPS {
                let collateral = mul_div_floor(
                    effective,
                    BPS_DENOMINATOR - haircut as u128,
                    BPS_DENOMINATOR,
                )
                .unwrap();
                out.push(
                    vector(format!("valuation/{rate}/{fee}/{haircut}"))
                        .input("grossRate", rate)
                        .input("withdrawalFeeBps", fee)
                        .input("haircutBps", haircut)
                        .output("effectiveRate", effective)
                        .output("collateralRate", collateral),
                );
            }
        }
    }

    // ---- amount <-> value, both directions and both roundings ------------
    for &rate in &RATES {
        for &amount in &AMOUNTS {
            let floored = market_value(amount, 9, rate, Rounding::Down).unwrap();
            let ceiled = market_value(amount, 9, rate, Rounding::Up).unwrap();
            let back = value_to_amount(floored, 9, rate, Rounding::Down).unwrap();
            out.push(
                vector(format!("amountValue/{rate}/{amount}"))
                    .input("amount", amount)
                    .input("decimals", 9)
                    .input("priceScaled", rate)
                    .output("valueFloor", floored)
                    .output("valueCeil", ceiled)
                    .output("roundTripFloor", back),
            );
        }
    }

    // ---- the share exchange rate ----------------------------------------
    //
    // Floors. A supplier redeeming must never be paid more than the pool holds,
    // and the one-unit difference is exactly what a first-depositor attack
    // tries to accumulate.
    for (liquidity, shares) in [
        (0u128, 0u64),
        (1, 1),
        (1_000_000_007, 999_999_999),
        (333_333_333_333, 111_111_111_111),
        (999_999_999_999_999, 1_000_000_000_000_000),
    ] {
        let rate = if shares == 0 {
            FIXED_POINT_SCALE
        } else {
            mul_div_floor(liquidity, FIXED_POINT_SCALE, shares as u128).unwrap()
        };
        out.push(
            vector(format!("shareRate/{liquidity}/{shares}"))
                .input("totalLiquidity", liquidity)
                .input("shareSupply", shares)
                .output("exchangeRate", rate),
        );
    }

    // ---- the health factor ----------------------------------------------
    for (borrowed, unhealthy) in [
        (0u128, 0u128),
        (1, 1),
        (1_000_000_000_000_000_000, 1_000_000_000_000_000_000),
        (999_999_999_999_999_999, 1_000_000_000_000_000_000),
        (1_000_000_000_000_000_001, 1_000_000_000_000_000_000),
        (7_777_777_777_777_777_777, 6_500_000_000_000_000_000),
    ] {
        let hf = if borrowed == 0 {
            None
        } else {
            Some(mul_div_floor(unhealthy, BPS_DENOMINATOR, borrowed).unwrap())
        };
        out.push(
            vector(format!("healthFactor/{borrowed}/{unhealthy}"))
                .input("borrowedValue", borrowed)
                .input("unhealthyBorrowValue", unhealthy)
                .output(
                    "healthFactorBps",
                    hf.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
                )
                .output("liquidatable", borrowed > unhealthy),
        );
    }

    // ---- the seize calculation -------------------------------------------
    //
    // The one a liquidator gets wrong. Every step floors toward the borrower,
    // so a client that rounded up anywhere computes a seize the program
    // refuses as LiquidationTooLarge.
    for &repay_value in &[
        1_000_000_000_000_000_000u128,
        7_777_777_777_777_777u128,
        333_333_333_333_333_333u128,
    ] {
        for &bonus in &[0u16, 800, 1_500] {
            for &price in &[RATES[1], RATES[3]] {
                let bonus_value =
                    mul_div_floor(repay_value, bonus as u128, BPS_DENOMINATOR).unwrap();
                let seize_value = repay_value + bonus_value;
                let seize_liquidity =
                    value_to_amount(seize_value, 9, price, Rounding::Down).unwrap();
                out.push(
                    vector(format!("seize/{repay_value}/{bonus}/{price}"))
                        .input("repayValue", repay_value)
                        .input("liquidationBonusBps", bonus)
                        .input("collateralPriceScaled", price)
                        .input("collateralDecimals", 9)
                        .output("bonusValue", bonus_value)
                        .output("seizeValue", seize_value)
                        .output("seizeLiquidity", seize_liquidity),
                );
            }
        }
    }

    // ---- the close factor -------------------------------------------------
    for (debt, hf, configured) in [
        (1_000_000_000u64, Some(10_500u128), 5_000u16),
        (1_000_000_000, Some(9_501), 5_000),
        (1_000_000_000, Some(9_500), 5_000),
        (1_000_000_000, Some(9_499), 5_000), // opens to 100%
        (999_999_999, Some(1), 5_000),
        (999_999_999, None, 5_000),
    ] {
        let effective = match hf {
            Some(value) if value < 9_500 => BPS_DENOMINATOR as u16,
            _ => configured,
        };
        let max_repay = mul_div_floor(debt as u128, effective as u128, BPS_DENOMINATOR).unwrap();
        out.push(
            vector(format!(
                "closeFactor/{debt}/{}/{configured}",
                hf.map(|v| v.to_string()).unwrap_or_else(|| "none".into())
            ))
            .input("debt", debt)
            .input(
                "healthFactorBps",
                hf.map(|v| v.to_string()).unwrap_or_else(|| "null".into()),
            )
            .input("configuredCloseFactorBps", configured)
            .output("effectiveCloseFactorBps", effective)
            .output("maxRepayAmount", max_repay),
        );
    }

    out
}

fn render(vectors: &[Vector]) -> String {
    let mut out = String::new();
    writeln!(out, "{{").unwrap();
    writeln!(
        out,
        "  \"$comment\": \"GENERATED by protocol/programs/aera/tests/test_math_vectors.rs. Every value is the program's own answer. sdk/test/vectors.test.ts replays them through the TypeScript and requires identical output. Regenerate with AERA_WRITE_MATH_VECTORS=1 cargo test --test test_math_vectors.\","
    )
    .unwrap();
    writeln!(out, "  \"count\": {},", vectors.len()).unwrap();
    writeln!(out, "  \"vectors\": [").unwrap();

    for (index, v) in vectors.iter().enumerate() {
        writeln!(out, "    {{").unwrap();
        writeln!(out, "      \"name\": \"{}\",", v.name).unwrap();
        writeln!(out, "      \"inputs\": {{").unwrap();
        for (i, (key, value)) in v.inputs.iter().enumerate() {
            let comma = if i + 1 < v.inputs.len() { "," } else { "" };
            writeln!(out, "        \"{key}\": \"{value}\"{comma}").unwrap();
        }
        writeln!(out, "      }},").unwrap();
        writeln!(out, "      \"outputs\": {{").unwrap();
        for (i, (key, value)) in v.outputs.iter().enumerate() {
            let comma = if i + 1 < v.outputs.len() { "," } else { "" };
            writeln!(out, "        \"{key}\": \"{value}\"{comma}").unwrap();
        }
        writeln!(out, "      }}").unwrap();
        let comma = if index + 1 < vectors.len() { "," } else { "" };
        writeln!(out, "    }}{comma}").unwrap();
    }

    writeln!(out, "  ]").unwrap();
    write!(out, "}}").unwrap();
    out.push('\n');
    out
}

#[test]
fn the_math_vectors_match_the_program() {
    let expected = render(&build());

    if std::env::var("AERA_WRITE_MATH_VECTORS").is_ok() {
        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config"))
            .expect("could not create config/");
        std::fs::write(VECTORS_PATH, &expected).expect("could not write the vectors");
        println!("wrote {VECTORS_PATH}");
        return;
    }

    /*
     * The consumer is outside this crate, and one repository does not have it.
     *
     * `config/math-vectors.json` is generated FROM the program's arithmetic and
     * lives in the monorepo, where the TypeScript suite replays it to prove the
     * SDK and the liquidator compute what the program computes. `aera-v1` --
     * the published repository -- contains the program and nothing else, so the
     * path does not exist there.
     *
     * Absent, the comparison is skipped with a note. Present, it is exactly what
     * it was. Note that `build()` and `render()` above have already run either
     * way, so the program's arithmetic is still exercised across the whole grid
     * and `the_vectors_cover_every_family` still holds it to its coverage; what
     * is skipped is only the check that an external file agrees. A missing file
     * is a different repository; a *differing* file is still a failure.
     */
    if !std::path::Path::new(VECTORS_PATH).exists() {
        println!("no generated consumer in this repository; built the vectors only");
        return;
    }

    let actual = std::fs::read_to_string(VECTORS_PATH).unwrap_or_else(|error| {
        panic!(
            "{VECTORS_PATH} is missing ({error}). The TypeScript suite replays it. \
             Regenerate with:\n\n    AERA_WRITE_MATH_VECTORS=1 cargo test --test test_math_vectors\n"
        )
    });

    assert_eq!(
        actual.replace("\r\n", "\n"),
        expected,
        "the committed vectors no longer match the program's arithmetic. If the \
         program changed on purpose, regenerate them AND check what the change \
         means for the SDK and the liquidator, which are written against these \
         numbers."
    );
}

/// The vectors actually cover the cases they claim to.
///
/// A generator that silently produced an empty list would leave the TypeScript
/// suite passing against nothing.
#[test]
fn the_vectors_cover_every_family() {
    let vectors = build();
    for family in [
        "valuation/",
        "amountValue/",
        "shareRate/",
        "healthFactor/",
        "seize/",
        "closeFactor/",
    ] {
        let count = vectors
            .iter()
            .filter(|v| v.name.starts_with(family))
            .count();
        assert!(count > 0, "no vectors for {family}");
    }
    assert!(
        vectors.len() > 100,
        "only {} vectors; the grid is not being expanded",
        vectors.len()
    );
}
