//! Exact money, and how to divide it without losing any.
//!
//! `docs/QUERY.md` section 4.1: "A decimal never becomes a float in an
//! aggregate. Money keeps its exact value." Attribution divides a conversion's
//! value between the touches that earned it, and a weight is a ratio, so the
//! division is the one place in this system where money meets a fraction.
//!
//! # The rule this module exists for
//!
//! **The credited parts add up to the whole, exactly.** A report where the
//! campaign column totals 19.98 and the conversion it came from was 19.99 is a
//! report somebody has to explain, and the explanation is always the same
//! rounding that produced it.
//!
//! Dividing 19.99 three ways cannot give three exact thirds at any scale, so
//! this does not pretend to. It converts the value to whole units at a working
//! scale, divides those units by the largest-remainder rule, and hands out
//! whole units. Every part is exact, every part is as near its weight as a whole
//! unit allows, and the parts add up to the value that was divided.
//!
//! # The working scale
//!
//! Six decimal places past the value's own scale. A currency with two decimals
//! divided three ways therefore carries four more digits than the money did, so
//! the rounding a person could notice happens once, when the report is read,
//! rather than at each of the divisions under it.

/// How many digits past the value's own scale a division carries.
pub const GUARD_DIGITS: u32 = 6;

/// The most decimal places an amount may carry.
///
/// A value with more digits than this is refused rather than truncated, because
/// truncating money is exactly the failure this module exists to prevent.
pub const MAX_SCALE: u32 = 18;

/// An exact decimal amount: a whole number of units at a scale.
///
/// `Amount { units: 1999, scale: 2 }` is 19.99. Two amounts of different scales
/// compare and add by lifting the coarser one, and neither ever becomes a float.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Amount {
    pub units: i128,
    pub scale: u32,
}

impl Amount {
    pub const ZERO: Amount = Amount { units: 0, scale: 0 };

    pub fn new(units: i128, scale: u32) -> Amount {
        Amount { units, scale }
    }

    /// Read an exact decimal from its text form.
    ///
    /// It returns `None` rather than a wrong number for anything it cannot read,
    /// including a value with more decimal places than [`MAX_SCALE`] and one
    /// whose digits overflow. A caller turns that into a named refusal.
    pub fn parse(text: &str) -> Option<Amount> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let (negative, rest) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, fraction) = rest.split_once('.').unwrap_or((rest, ""));
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        if fraction.len() as u32 > MAX_SCALE {
            return None;
        }
        let mut units: i128 = 0;
        for character in whole.chars().chain(fraction.chars()) {
            let digit = character.to_digit(10)? as i128;
            units = units.checked_mul(10)?.checked_add(digit)?;
        }
        Some(Amount {
            units: if negative { -units } else { units },
            scale: fraction.len() as u32,
        })
    }

    /// The same amount at a finer scale.
    pub fn at_scale(&self, scale: u32) -> Option<Amount> {
        if scale < self.scale {
            return None;
        }
        let factor = 10i128.checked_pow(scale - self.scale)?;
        Some(Amount {
            units: self.units.checked_mul(factor)?,
            scale,
        })
    }

    pub fn add(&self, other: &Amount) -> Option<Amount> {
        let scale = self.scale.max(other.scale);
        let left = self.at_scale(scale)?;
        let right = other.at_scale(scale)?;
        Some(Amount {
            units: left.units.checked_add(right.units)?,
            scale,
        })
    }

    pub fn is_zero(&self) -> bool {
        self.units == 0
    }

    /// The canonical text form, with no trailing zeros.
    ///
    /// `19.990000` and `19.99` are one amount, and a report that printed the
    /// first would not compare equal to a ledger that holds the second.
    pub fn to_text(&self) -> String {
        if self.scale == 0 {
            return self.units.to_string();
        }
        let negative = self.units < 0;
        let digits = self.units.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let padded = if digits.len() <= scale {
            format!("{}{digits}", "0".repeat(scale + 1 - digits.len()))
        } else {
            digits
        };
        let split = padded.len() - scale;
        let mut fraction = padded[split..].to_string();
        while fraction.ends_with('0') {
            fraction.pop();
        }
        let whole = &padded[..split];
        let body = if fraction.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{fraction}")
        };
        // A negative zero is zero. `-0.00` renders as `0`, so two amounts that
        // are the same number read the same.
        if negative && self.units != 0 {
            format!("-{body}")
        } else {
            body
        }
    }

    /// Divide this amount between weights, losing nothing.
    ///
    /// The parts add up to this amount exactly. Each part is the largest whole
    /// unit at the working scale that is not more than its share, and the units
    /// left over go to the largest remainders, one each, in weight order. That
    /// is the same rule an election uses to hand out whole seats from
    /// fractional shares, and it exists here for the same reason: the total is
    /// fixed and the parts have to add up to it.
    ///
    /// A weight that is not a positive number contributes nothing. Weights that
    /// are all zero divide evenly, because a caller that asked for a division
    /// meant a division.
    pub fn split(&self, weights: &[f64]) -> Vec<Amount> {
        if weights.is_empty() {
            return Vec::new();
        }
        // The working scale, or this amount's own when lifting would overflow.
        // Dividing at a coarser scale loses precision; it does not lose money,
        // because the largest-remainder rule below still hands out every unit.
        let working = self
            .at_scale((self.scale + GUARD_DIGITS).min(MAX_SCALE))
            .unwrap_or(*self);
        self.split_of(working, weights)
    }

    fn split_of(&self, working: Amount, weights: &[f64]) -> Vec<Amount> {
        let Amount {
            units: total,
            scale,
        } = working;
        if weights.is_empty() {
            return Vec::new();
        }

        // **The division is integer arithmetic from here on.** It was floating
        // point: `floor(magnitude as f64 * share)`, and then one unit at a time
        // for whatever was left. An `f64` holds 15 digits, so for a 28-digit
        // amount the floors missed the total by about ten to the thirteenth,
        // and the loop that handed the difference out one unit at a time ran
        // for that many turns. One conversion with a long value, sent by an
        // end user, kept every attribution query of its project spinning.
        //
        // A weight becomes a whole number of parts in `WEIGHT_PARTS`, which is
        // all the precision an `f64` weight has. Every part below is then an
        // exact floor, and what is left over is less than one unit for each
        // weight.
        const WEIGHT_PARTS: f64 = 1e15;
        let cleaned: Vec<f64> = weights
            .iter()
            .map(|w| if w.is_finite() && *w > 0.0 { *w } else { 0.0 })
            .collect();
        let sum: f64 = cleaned.iter().sum();
        let mut whole: Vec<u128> = if sum > 0.0 && sum.is_finite() {
            cleaned
                .iter()
                .map(|w| (w / sum * WEIGHT_PARTS).round() as u128)
                .collect()
        } else {
            vec![1; cleaned.len()]
        };
        if whole.iter().all(|w| *w == 0) {
            whole = vec![1; cleaned.len()];
        }
        let all: u128 = whole.iter().sum();

        // The sign travels with the total rather than through the division, so
        // a refund divides the same way a sale does.
        let magnitude = total.unsigned_abs();
        let sign: i128 = if total < 0 { -1 } else { 1 };

        // `magnitude * weight / all` without the product, which does not fit.
        // With `magnitude = q * all + r`, the quotient is `q * weight` plus
        // `r * weight / all`, and `r * weight` fits because both are small.
        let (quotient, rest) = (magnitude / all, magnitude % all);
        let mut parts: Vec<u128> = Vec::with_capacity(whole.len());
        let mut remainders: Vec<u128> = Vec::with_capacity(whole.len());
        for weight in &whole {
            let fine = rest.saturating_mul(*weight);
            parts.push(quotient.saturating_mul(*weight).saturating_add(fine / all));
            remainders.push(fine % all);
        }
        let handed_out: u128 = parts
            .iter()
            .fold(0u128, |sum, part| sum.saturating_add(*part));
        let left = magnitude.saturating_sub(handed_out);

        // The remainders, largest first. A tie goes to the earlier touch, which
        // is a rule rather than whatever the sort happened to do.
        let mut order: Vec<usize> = (0..parts.len()).collect();
        // Between equal remainders the larger weight comes first. Two weights
        // closer than one part in `WEIGHT_PARTS` have equal remainders here, and
        // the one a person wrote as larger must not lose the unit to its place
        // in the list.
        order.sort_by(|a, b| {
            remainders[*b]
                .cmp(&remainders[*a])
                .then(
                    cleaned[*b]
                        .partial_cmp(&cleaned[*a])
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(a.cmp(b))
        });
        // What is left is less than one unit for each weight, so this is one
        // unit each for the largest remainders. It is a division and not a
        // loop over units, so its cost never follows the amount.
        let count = order.len() as u128;
        let (each, extra) = (left / count, (left % count) as usize);
        for (rank, index) in order.iter().enumerate() {
            parts[*index] = parts[*index]
                .saturating_add(each)
                .saturating_add(u128::from(rank < extra));
        }

        parts
            .into_iter()
            .map(|units| Amount {
                units: i128::try_from(units).unwrap_or(i128::MAX) * sign,
                scale,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_amount_longer_than_a_float_can_hold_divides_at_once_and_adds_up() {
        // 28 digits. The floating-point division missed this total by about
        // ten to the thirteenth units and then handed them out one at a time.
        let amount = Amount::parse("1234567890123456789012345678").unwrap();
        let parts = amount.split(&[0.4, 0.4, 0.2]);
        let sum = parts
            .iter()
            .fold(Amount::ZERO, |sum, part| sum.add(part).expect("it adds"));
        assert_eq!(
            sum.to_text(),
            amount.to_text(),
            "the parts do not add up to the amount"
        );
        // The shares are what the weights say, to the precision a weight has.
        assert_eq!(&parts[0].to_text()[..10], "4938271560");
        assert_eq!(parts[0], parts[1]);

        // The largest amount there is, and a refund of it.
        for units in [i128::MAX, i128::MIN + 1] {
            let parts = Amount::new(units, 0).split(&[1.0, 1.0, 1.0]);
            let sum: i128 = parts.iter().map(|part| part.units).sum();
            assert_eq!(sum, units);
        }
    }

    #[test]
    fn text_survives_a_round_trip_without_growing_zeros() {
        for text in ["19.99", "0", "1000", "-4.5", "0.000001"] {
            assert_eq!(Amount::parse(text).unwrap().to_text(), text, "{text}");
        }
    }

    #[test]
    fn a_value_nobody_can_read_is_none_rather_than_a_wrong_number() {
        assert_eq!(Amount::parse(""), None);
        assert_eq!(Amount::parse("nineteen"), None);
        assert_eq!(Amount::parse("19.9.9"), None);
        // More decimal places than the module carries. Truncating money is the
        // failure this refuses.
        assert_eq!(Amount::parse(&format!("0.{}", "1".repeat(19))), None);
    }

    #[test]
    fn three_equal_touches_divide_nineteen_ninety_nine_and_lose_nothing() {
        // The fixture a person can check: 19.99 cannot be divided into three
        // exact thirds at any scale, so the test is that the three parts add
        // back up to 19.99 and that no two of them differ by more than one unit.
        let value = Amount::parse("19.99").unwrap();
        let parts = value.split(&[1.0, 1.0, 1.0]);
        assert_eq!(parts.len(), 3);

        let total = parts
            .iter()
            .fold(Amount::ZERO, |sum, part| sum.add(part).unwrap());
        assert_eq!(total.to_text(), "19.99");

        let largest = parts.iter().map(|p| p.units).max().unwrap();
        let smallest = parts.iter().map(|p| p.units).min().unwrap();
        assert!(largest - smallest <= 1, "{largest} against {smallest}");
    }

    #[test]
    fn a_position_split_gives_the_ends_what_they_were_promised() {
        // 100 divided 40 / 20 / 40 is exact, so the parts are exactly those.
        let parts = Amount::parse("100").unwrap().split(&[0.4, 0.2, 0.4]);
        assert_eq!(
            parts.iter().map(Amount::to_text).collect::<Vec<_>>(),
            vec!["40".to_string(), "20".to_string(), "40".to_string()]
        );
    }

    #[test]
    fn weights_that_do_not_add_up_to_one_are_shares_rather_than_amounts() {
        // A caller passing 2 and 2 means half each, not two units each.
        let parts = Amount::parse("10").unwrap().split(&[2.0, 2.0]);
        assert_eq!(
            parts.iter().map(Amount::to_text).collect::<Vec<_>>(),
            vec!["5".to_string(), "5".to_string()]
        );
    }

    #[test]
    fn every_weight_zero_divides_evenly_rather_than_giving_nothing_away() {
        let parts = Amount::parse("9").unwrap().split(&[0.0, 0.0, 0.0]);
        let total = parts
            .iter()
            .fold(Amount::ZERO, |sum, part| sum.add(part).unwrap());
        assert_eq!(total.to_text(), "9");
    }

    #[test]
    fn a_refund_divides_the_way_a_sale_does() {
        let parts = Amount::parse("-19.99").unwrap().split(&[1.0, 1.0, 1.0]);
        let total = parts
            .iter()
            .fold(Amount::ZERO, |sum, part| sum.add(part).unwrap());
        assert_eq!(total.to_text(), "-19.99");
    }

    #[test]
    fn nothing_divides_into_nothing() {
        let parts = Amount::parse("0").unwrap().split(&[1.0, 3.0]);
        assert!(parts.iter().all(Amount::is_zero));
    }
}
