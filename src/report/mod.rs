//! Rendering. Formats numbers the engine produced; contains no pricing arithmetic.

pub mod table;

use rust_decimal::{Decimal, RoundingStrategy};

use crate::model::Report;

pub fn json(report: &Report) -> serde_json::Result<String> {
    serde_json::to_string_pretty(report)
}

/// `1284.416` becomes `$1,284.42`. Rounding happens here, at display time only.
pub fn money(amount: Decimal) -> String {
    let rounded = amount.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    let text = format!("{:.2}", rounded.abs());
    let (whole, cents) = text.split_once('.').unwrap_or((&text, "00"));

    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }

    let sign = if rounded.is_sign_negative() && !rounded.is_zero() {
        "-"
    } else {
        ""
    };
    format!("{sign}${grouped}.{cents}")
}

/// Like `money`, with an explicit `+` for increases.
pub fn signed_money(amount: Decimal) -> String {
    let text = money(amount);
    if text.starts_with('-') || amount.round_dp(2).is_zero() {
        return text;
    }
    format!("+{text}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rust_decimal::dec;

    #[test]
    fn money_formats_groups_signs_and_rounds_half_away_from_zero() {
        assert_eq!(money(dec!(0)), "$0.00");
        assert_eq!(money(dec!(1284.416)), "$1,284.42");
        assert_eq!(money(dec!(0.005)), "$0.01");
        assert_eq!(money(dec!(0.0049)), "$0.00");
        assert_eq!(money(dec!(-0.001)), "$0.00");
        assert_eq!(money(dec!(1234567.895)), "$1,234,567.90");
        assert_eq!(money(dec!(-350.24)), "-$350.24");
        assert_eq!(signed_money(dec!(350.24)), "+$350.24");
        assert_eq!(signed_money(dec!(-4202.88)), "-$4,202.88");
        assert_eq!(signed_money(dec!(0)), "$0.00");
    }

    proptest! {
        #[test]
        fn formatted_money_parses_back_to_within_half_a_cent(cents in -10_000_000_000i64..10_000_000_000, extra in 0u32..1000) {
            let amount = Decimal::new(cents, 2) + Decimal::new(i64::from(extra), 5);
            let parsed: Decimal = money(amount).replace(['$', ','], "").parse().unwrap();
            prop_assert!((parsed - amount).abs() <= dec!(0.005));
        }
    }
}
