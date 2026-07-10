use rust_decimal::Decimal;
use rust_decimal::prelude::*;

/// Calculates the monthly payment (PMT) for a loan using the standard amortization formula.
///
/// Formula:
/// PMT = P * [r * (1 + r)^n] / [(1 + r)^n - 1]
///
/// Where:
/// - P = principal_amount
/// - r = monthly_rate (annual_rate / 12)
/// - n = amortization_months
pub fn calculate_monthly_payment(
    principal: Decimal,
    annual_rate: Decimal,
    months: u32,
) -> Option<Decimal> {
    if principal <= Decimal::ZERO || annual_rate < Decimal::ZERO || months == 0 {
        return None;
    }

    // If interest rate is exactly 0, the payment is just principal / months.
    if annual_rate == Decimal::ZERO {
        let months_dec = Decimal::from(months);
        return Some((principal / months_dec).round_dp(2));
    }

    // Convert values to f64 to perform floating-point exponentiation.
    let p_f64 = principal.to_f64()?;
    let r_annual_f64 = annual_rate.to_f64()?;
    let r_monthly_f64 = r_annual_f64 / 12.0;
    let n_f64 = months as f64;

    // Standard PMT calculation:
    // PMT = P * r * (1 + r)^n / ((1 + r)^n - 1)
    let one_plus_r_n = (1.0 + r_monthly_f64).powf(n_f64);
    let pmt_f64 = p_f64 * (r_monthly_f64 * one_plus_r_n) / (one_plus_r_n - 1.0);

    // Convert back to Decimal and round to 2 decimal places.
    Decimal::from_f64(pmt_f64).map(|d| d.round_dp(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_calculate_monthly_payment_interest_free() {
        let principal = Decimal::from(1200);
        let annual_rate = Decimal::ZERO;
        let months = 12;

        let payment = calculate_monthly_payment(principal, annual_rate, months);
        assert_eq!(payment, Some(Decimal::from(100)));
    }

    #[test]
    fn test_calculate_monthly_payment_standard() {
        // $10,000 loan at 8.5% annual interest rate over 24 months.
        let principal = Decimal::from(10000);
        let annual_rate = Decimal::from_str("0.085").unwrap();
        let months = 24;

        let payment = calculate_monthly_payment(principal, annual_rate, months);
        // Expected value based on standard PMT formula: 454.56
        assert_eq!(payment, Some(Decimal::from_str("454.56").unwrap()));
    }

    #[test]
    fn test_calculate_monthly_payment_invalid_inputs() {
        let principal = Decimal::from(10000);
        let annual_rate = Decimal::from_str("0.085").unwrap();
        
        // Zero months
        assert_eq!(calculate_monthly_payment(principal, annual_rate, 0), None);

        // Negative principal
        assert_eq!(calculate_monthly_payment(Decimal::from(-100), annual_rate, 12), None);

        // Zero principal
        assert_eq!(calculate_monthly_payment(Decimal::ZERO, annual_rate, 12), None);

        // Negative rate
        assert_eq!(calculate_monthly_payment(principal, Decimal::from_str("-0.05").unwrap(), 12), None);
    }
}
