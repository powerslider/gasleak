//! Shared USD formatting helpers used by the table renderer and the Slack
//! reporter. Two presentations:
//!
//! - [`usd_cents`]: always two-decimal, with thousand separators. Matches
//!   how spreadsheets render money; used in table footers where every cent
//!   is expected.
//! - [`usd_compact`]: rounds to whole dollars for amounts at or above $100,
//!   keeps cents below. Reads cleanly on mobile Slack where the sentence
//!   shape matters more than rounding precision.
//!
//! Both clamp non-finite values to `"-"` / `"$—"` so garbage data never
//! becomes garbage output.

/// Two-decimal USD with thousand separators: `$18,720.00`, `-$42.50`.
/// Non-finite → `"-"`.
pub fn usd_cents(amount: f64) -> String {
    if !amount.is_finite() {
        return "-".to_string();
    }
    let cents = (amount * 100.0).round() as i64;
    let sign = if cents < 0 { "-" } else { "" };
    let cents_abs = cents.unsigned_abs();
    let dollars = cents_abs / 100;
    let frac = cents_abs % 100;
    format!("{sign}${}.{frac:02}", thousands(dollars))
}

/// Compact USD: whole dollars with thousand separators above $100
/// (`$18,720`), two-decimal below (`$42.50`). Non-finite → `"$—"`.
pub fn usd_compact(amount: f64) -> String {
    if !amount.is_finite() {
        return "$—".to_string();
    }
    let sign = if amount < 0.0 { "-" } else { "" };
    if amount.abs() >= 100.0 {
        let dollars = amount.abs().round() as u64;
        format!("{sign}${}", thousands(dollars))
    } else {
        format!("{sign}${:.2}", amount.abs())
    }
}

fn thousands(v: u64) -> String {
    let mut out = String::new();
    for (i, c) in v.to_string().chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.insert(0, ',');
        }
        out.insert(0, c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usd_cents_adds_thousand_separators() {
        assert_eq!(usd_cents(0.0), "$0.00");
        assert_eq!(usd_cents(9.99), "$9.99");
        assert_eq!(usd_cents(999.5), "$999.50");
        assert_eq!(usd_cents(1_000.0), "$1,000.00");
        assert_eq!(usd_cents(18_720.00), "$18,720.00");
        assert_eq!(usd_cents(226_200.00), "$226,200.00");
        assert_eq!(usd_cents(1_234_567.89), "$1,234,567.89");
    }

    #[test]
    fn usd_cents_handles_negative_and_non_finite() {
        assert_eq!(usd_cents(-42.50), "-$42.50");
        assert_eq!(usd_cents(f64::NAN), "-");
        assert_eq!(usd_cents(f64::INFINITY), "-");
    }

    #[test]
    fn usd_cents_rounds_to_cents() {
        let out = usd_cents(0.005);
        // f64::round is away-from-zero for exact halves; allow either.
        assert!(out == "$0.01" || out == "$0.00", "got {out}");
        assert_eq!(usd_cents(1.234), "$1.23");
        assert_eq!(usd_cents(1.239), "$1.24");
    }

    #[test]
    fn usd_compact_rounds_past_hundred_keeps_cents_below() {
        assert_eq!(usd_compact(42.50), "$42.50");
        assert_eq!(usd_compact(99.99), "$99.99");
        // Exact boundary: 100.0 rounds to whole dollars.
        assert_eq!(usd_compact(100.0), "$100");
        assert_eq!(usd_compact(18_720.33), "$18,720");
        assert_eq!(usd_compact(1_234_567.89), "$1,234,568");
    }

    #[test]
    fn usd_compact_handles_negative_and_non_finite() {
        assert_eq!(usd_compact(-500.0), "-$500");
        assert_eq!(usd_compact(-1.75), "-$1.75");
        assert_eq!(usd_compact(f64::NAN), "$—");
        assert_eq!(usd_compact(f64::INFINITY), "$—");
    }
}
