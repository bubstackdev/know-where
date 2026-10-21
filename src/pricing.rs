//! Decimal accounting in micro-USD. A token times USD/million tokens is
//! already micro-USD, so estimated costs need no division or float arithmetic.
use crate::types::{Price, Usage};
use rust_decimal::{Decimal, RoundingStrategy, prelude::ToPrimitive};
use std::str::FromStr;

/// Price the five token categories, rounding the combined total once.
/// One-hour writes are included in total cache writes, not additional tokens.
/// Like the reference, arithmetic/conversion overflow records i64::MAX.
pub fn estimated(price: &Price, usage: &Usage) -> i64 {
    estimated_decimal(price, usage)
        .map(round_micros)
        .unwrap_or(i64::MAX)
}

fn estimated_decimal(price: &Price, usage: &Usage) -> Option<Decimal> {
    let ordinary_writes = Decimal::from(usage.cache_write_tokens)
        .checked_sub(Decimal::from(usage.cache_write1h_tokens))?;
    let terms = [
        (Decimal::from(usage.input_tokens), price.input),
        (Decimal::from(usage.output_tokens), price.output),
        (Decimal::from(usage.cache_read_tokens), price.cache_read),
        (ordinary_writes, price.cache_write),
        (
            Decimal::from(usage.cache_write1h_tokens),
            price.cache_write1h.unwrap_or(price.cache_write),
        ),
    ];
    terms
        .into_iter()
        .try_fold(Decimal::ZERO, |total, (tokens, rate)| {
            // An unused rate cannot overflow the cost of zero tokens.
            if tokens.is_zero() {
                return Some(total);
            }
            total.checked_add(tokens.checked_mul(decimal(rate)?)?)
        })
}

/// Convert a provider's reported USD amount to integer micro-USD.
pub fn reported(dollars: f64) -> i64 {
    decimal(dollars)
        .and_then(|amount| amount.checked_mul(Decimal::from(1_000_000u64)))
        .map(round_micros)
        .unwrap_or(i64::MAX)
}

fn decimal(value: f64) -> Option<Decimal> {
    if !value.is_finite() {
        return None;
    }
    // This recovers the intended decimal rate from the current f64 schema,
    // rather than embedding its binary approximation in a Decimal.
    Decimal::from_str(&value.to_string()).ok().or_else(|| {
        // Below Decimal's scale, even u64::MAX tokens cannot reach half a
        // micro-dollar. Treat underflow as zero, while overflow saturates.
        (value.abs() < 1e-28).then_some(Decimal::ZERO)
    })
}

fn round_micros(value: Decimal) -> i64 {
    value
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .to_i64()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_rates_round_half_micros_away_from_zero() {
        let price = Price {
            input: 0.29,
            ..Default::default()
        };
        assert_eq!(
            estimated(
                &price,
                &Usage {
                    input_tokens: 50,
                    ..Default::default()
                }
            ),
            15
        );
        assert_eq!(
            estimated(
                &Price {
                    input: 0.58,
                    ..Default::default()
                },
                &Usage {
                    input_tokens: 25,
                    ..Default::default()
                }
            ),
            15
        );
    }

    #[test]
    fn token_categories_are_summed_before_rounding() {
        let price = Price {
            input: 0.25,
            output: 0.25,
            ..Default::default()
        };
        assert_eq!(
            estimated(
                &price,
                &Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                }
            ),
            1
        );
    }

    #[test]
    fn reported_dollars_use_decimal_half_micro_rounding() {
        assert_eq!(reported(0.0000145), 15);
        assert_eq!(reported(0.00001449), 14);
        assert_eq!(reported(0.0000005), 1);
        assert_eq!(reported(0.0), 0);
        assert_eq!(reported(-0.0000145), -15);
    }

    #[test]
    fn one_hour_cache_writes_are_a_subset_with_their_own_price() {
        let usage = Usage {
            cache_write_tokens: 100,
            cache_write1h_tokens: 20,
            ..Default::default()
        };
        let price = Price {
            cache_write: 2.,
            cache_write1h: Some(5.),
            ..Default::default()
        };
        assert_eq!(estimated(&price, &usage), 260);
        assert_eq!(
            estimated(
                &Price {
                    cache_write1h: None,
                    ..price
                },
                &usage
            ),
            200
        );
    }

    #[test]
    fn inconsistent_cache_counts_keep_reference_signed_subtraction() {
        let usage = Usage {
            cache_write_tokens: 1,
            cache_write1h_tokens: 2,
            ..Default::default()
        };
        assert_eq!(
            estimated(
                &Price {
                    cache_write: 2.,
                    cache_write1h: Some(5.),
                    ..Default::default()
                },
                &usage
            ),
            8
        );
    }

    #[test]
    fn decimal_or_integer_overflow_saturates_without_panicking() {
        assert_eq!(
            estimated(
                &Price {
                    input: f64::MAX,
                    ..Default::default()
                },
                &Usage {
                    input_tokens: 1,
                    ..Default::default()
                }
            ),
            i64::MAX
        );
        assert_eq!(
            estimated(
                &Price {
                    input: 1e12,
                    ..Default::default()
                },
                &Usage {
                    input_tokens: u64::MAX,
                    ..Default::default()
                }
            ),
            i64::MAX
        );
        assert_eq!(
            estimated(
                &Price {
                    input: 1.,
                    ..Default::default()
                },
                &Usage {
                    input_tokens: i64::MAX as u64,
                    ..Default::default()
                }
            ),
            i64::MAX
        );
        assert_eq!(reported(f64::MAX), i64::MAX);
    }

    #[test]
    fn tiny_rates_and_unused_huge_rates_do_not_create_overflow_costs() {
        assert_eq!(
            estimated(
                &Price {
                    input: 1e-100,
                    ..Default::default()
                },
                &Usage {
                    input_tokens: u64::MAX,
                    ..Default::default()
                }
            ),
            0
        );
        assert_eq!(reported(1e-100), 0);
        assert_eq!(
            estimated(
                &Price {
                    input: f64::MAX,
                    ..Default::default()
                },
                &Usage::default()
            ),
            0
        );
    }
}
