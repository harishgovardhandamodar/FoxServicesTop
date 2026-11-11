//! Residential electricity tariffs used by the Power tab's cost estimate.
//!
//! The Power tab's energy comes from the history database (`PowerView`
//! windows over 1 h to 24 h); this table only converts those joules into
//! money. Prices are approximate residential rates in euros per kWh
//! (2024–2025 averages, taxes included where they are normally billed) so
//! the tab can compare total consumption across countries. They are
//! display-only defaults, not billing data.

/// One country's residential electricity price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tariff {
    /// ISO-3166-ish selector code shown in the tab (`NL`, `DE`, ...).
    pub code: &'static str,
    /// Display name.
    pub country: &'static str,
    /// Approximate residential price in euros per kWh.
    pub price_eur_per_kwh: f64,
}

/// Selectable countries and their display-only tariffs, in a stable order.
/// The first entry is the default (`NL`).
pub const TARIFFS: &[Tariff] = &[
    Tariff {
        code: "NL",
        country: "Netherlands",
        price_eur_per_kwh: 0.30,
    },
    Tariff {
        code: "US",
        country: "United States",
        price_eur_per_kwh: 0.17,
    },
    Tariff {
        code: "CA",
        country: "Canada",
        price_eur_per_kwh: 0.13,
    },
    Tariff {
        code: "GB",
        country: "United Kingdom",
        price_eur_per_kwh: 0.32,
    },
    Tariff {
        code: "DE",
        country: "Germany",
        price_eur_per_kwh: 0.38,
    },
    Tariff {
        code: "FR",
        country: "France",
        price_eur_per_kwh: 0.27,
    },
    Tariff {
        code: "IT",
        country: "Italy",
        price_eur_per_kwh: 0.31,
    },
    Tariff {
        code: "ES",
        country: "Spain",
        price_eur_per_kwh: 0.28,
    },
    Tariff {
        code: "SE",
        country: "Sweden",
        price_eur_per_kwh: 0.21,
    },
    Tariff {
        code: "NO",
        country: "Norway",
        price_eur_per_kwh: 0.15,
    },
    Tariff {
        code: "PL",
        country: "Poland",
        price_eur_per_kwh: 0.23,
    },
    Tariff {
        code: "CH",
        country: "Switzerland",
        price_eur_per_kwh: 0.29,
    },
    Tariff {
        code: "AU",
        country: "Australia",
        price_eur_per_kwh: 0.24,
    },
    Tariff {
        code: "JP",
        country: "Japan",
        price_eur_per_kwh: 0.23,
    },
    Tariff {
        code: "KR",
        country: "South Korea",
        price_eur_per_kwh: 0.13,
    },
    Tariff {
        code: "IN",
        country: "India",
        price_eur_per_kwh: 0.07,
    },
    Tariff {
        code: "BR",
        country: "Brazil",
        price_eur_per_kwh: 0.19,
    },
    Tariff {
        code: "ZA",
        country: "South Africa",
        price_eur_per_kwh: 0.13,
    },
    Tariff {
        code: "SG",
        country: "Singapore",
        price_eur_per_kwh: 0.26,
    },
    Tariff {
        code: "AE",
        country: "UAE",
        price_eur_per_kwh: 0.08,
    },
];

/// Default tariff selector: the first entry (Netherlands, priced in euros).
pub const DEFAULT_TARIFF: usize = 0;

/// Tariff at `index`, wrapping around the table.
pub fn tariff(index: usize) -> Tariff {
    TARIFFS[index % TARIFFS.len()]
}

/// Cost in euros of `joules` at `price_eur_per_kwh`; `None` while the energy
/// is missing or unusable.
pub fn cost_eur(joules: Option<f64>, price_eur_per_kwh: f64) -> Option<f64> {
    let joules = joules?;
    if !joules.is_finite() || joules < 0.0 || !price_eur_per_kwh.is_finite() {
        return None;
    }
    Some(joules / 3_600_000.0 * price_eur_per_kwh)
}

/// Cost text (`€1.23`); `n/a` while the energy is missing.
pub fn format_cost_eur(joules: Option<f64>, price_eur_per_kwh: f64) -> String {
    match cost_eur(joules, price_eur_per_kwh) {
        Some(cost) if cost.is_finite() => format!("€{cost:.2}"),
        _ => super::units::NOT_AVAILABLE.to_owned(),
    }
}

/// Cost window labels the Power tab prices: the current timeline window,
/// the database windows, and `all` (everything the database holds, i.e.
/// the longest database window).
pub const COST_WINDOWS: [&str; 8] = ["window", "1h", "6h", "12h", "24h", "7d", "30d", "all"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_kwh_costs_the_tariff_price() {
        assert_eq!(cost_eur(Some(3_600_000.0), 0.30), Some(0.30));
        assert_eq!(format_cost_eur(Some(3_600_000.0), 0.30), "€0.30");
    }

    #[test]
    fn missing_or_bad_energy_has_no_cost() {
        assert_eq!(cost_eur(None, 0.30), None);
        assert_eq!(format_cost_eur(None, 0.30), "n/a");
        assert_eq!(cost_eur(Some(f64::NAN), 0.30), None);
        assert_eq!(cost_eur(Some(-1.0), 0.30), None);
    }

    #[test]
    fn every_tariff_has_a_positive_price() {
        assert!(!TARIFFS.is_empty());
        for tariff in TARIFFS {
            assert!(tariff.price_eur_per_kwh > 0.0, "{}", tariff.code);
        }
        assert_eq!(tariff(TARIFFS.len()).code, "NL", "index wraps");
    }

    #[test]
    fn the_default_is_the_netherlands_in_euros() {
        let tariff = tariff(DEFAULT_TARIFF);
        assert_eq!(tariff.code, "NL");
        assert_eq!(tariff.country, "Netherlands");
    }
}
