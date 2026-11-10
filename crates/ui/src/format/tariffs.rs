//! Residential electricity tariffs used by the Power tab's cost estimate.
//!
//! The Power tab's energy comes from the history database (`PowerView`
//! windows over 1 h to 24 h); this table only converts those joules into
//! money. Prices are approximate residential rates in USD per kWh
//! (2024–2025 averages, taxes included where they are normally billed) so
//! the tab can compare total consumption across countries. They are
//! display-only defaults, not billing data.

/// One country's residential electricity price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tariff {
    /// ISO-3166-ish selector code shown in the tab (`US`, `DE`, ...).
    pub code: &'static str,
    /// Display name.
    pub country: &'static str,
    /// Approximate residential price in USD per kWh.
    pub price_usd_per_kwh: f64,
}

/// Selectable countries and their display-only tariffs, in a stable order.
/// Index 0 is the default (`US`).
pub const TARIFFS: &[Tariff] = &[
    Tariff { code: "US", country: "United States", price_usd_per_kwh: 0.18 },
    Tariff { code: "CA", country: "Canada", price_usd_per_kwh: 0.14 },
    Tariff { code: "GB", country: "United Kingdom", price_usd_per_kwh: 0.34 },
    Tariff { code: "DE", country: "Germany", price_usd_per_kwh: 0.40 },
    Tariff { code: "FR", country: "France", price_usd_per_kwh: 0.28 },
    Tariff { code: "IT", country: "Italy", price_usd_per_kwh: 0.33 },
    Tariff { code: "ES", country: "Spain", price_usd_per_kwh: 0.29 },
    Tariff { code: "NL", country: "Netherlands", price_usd_per_kwh: 0.31 },
    Tariff { code: "SE", country: "Sweden", price_usd_per_kwh: 0.22 },
    Tariff { code: "NO", country: "Norway", price_usd_per_kwh: 0.16 },
    Tariff { code: "PL", country: "Poland", price_usd_per_kwh: 0.24 },
    Tariff { code: "CH", country: "Switzerland", price_usd_per_kwh: 0.30 },
    Tariff { code: "AU", country: "Australia", price_usd_per_kwh: 0.26 },
    Tariff { code: "JP", country: "Japan", price_usd_per_kwh: 0.25 },
    Tariff { code: "KR", country: "South Korea", price_usd_per_kwh: 0.14 },
    Tariff { code: "IN", country: "India", price_usd_per_kwh: 0.08 },
    Tariff { code: "BR", country: "Brazil", price_usd_per_kwh: 0.20 },
    Tariff { code: "ZA", country: "South Africa", price_usd_per_kwh: 0.14 },
    Tariff { code: "SG", country: "Singapore", price_usd_per_kwh: 0.28 },
    Tariff { code: "AE", country: "UAE", price_usd_per_kwh: 0.09 },
];

/// Tariff at `index`, wrapping around the table.
pub fn tariff(index: usize) -> Tariff {
    TARIFFS[index % TARIFFS.len()]
}

/// Cost in USD of `joules` at `price_usd_per_kwh`; `None` while the energy
/// is missing or unusable.
pub fn cost_usd(joules: Option<f64>, price_usd_per_kwh: f64) -> Option<f64> {
    let joules = joules?;
    if !joules.is_finite() || joules < 0.0 || !price_usd_per_kwh.is_finite() {
        return None;
    }
    Some(joules / 3_600_000.0 * price_usd_per_kwh)
}

/// Cost text (`$1.23`); `n/a` while the energy is missing.
pub fn format_cost_usd(joules: Option<f64>, price_usd_per_kwh: f64) -> String {
    match cost_usd(joules, price_usd_per_kwh) {
        Some(cost) if cost.is_finite() => format!("${cost:.2}"),
        _ => super::units::NOT_AVAILABLE.to_owned(),
    }
}

/// Cost window labels the Power tab prices: the current timeline window
/// plus the database windows it keeps (1 h to 24 h).
pub const COST_WINDOWS: [&str; 5] = ["window", "1h", "6h", "12h", "24h"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_kwh_costs_the_tariff_price() {
        assert_eq!(cost_usd(Some(3_600_000.0), 0.18), Some(0.18));
        assert_eq!(format_cost_usd(Some(3_600_000.0), 0.18), "$0.18");
    }

    #[test]
    fn missing_or_bad_energy_has_no_cost() {
        assert_eq!(cost_usd(None, 0.18), None);
        assert_eq!(format_cost_usd(None, 0.18), "n/a");
        assert_eq!(cost_usd(Some(f64::NAN), 0.18), None);
        assert_eq!(cost_usd(Some(-1.0), 0.18), None);
    }

    #[test]
    fn every_tariff_has_a_positive_price() {
        assert!(!TARIFFS.is_empty());
        for tariff in TARIFFS {
            assert!(tariff.price_usd_per_kwh > 0.0, "{}", tariff.code);
        }
        assert_eq!(tariff(0).code, "US");
        assert_eq!(tariff(TARIFFS.len()).code, "US", "index wraps");
    }
}
