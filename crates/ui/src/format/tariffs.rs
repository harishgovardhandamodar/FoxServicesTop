//! Residential electricity tariffs used by the Power tab's cost estimate.
//!
//! The Power tab's energy comes from the history database (`PowerView`
//! windows over 1 h to 24 h); this table only converts those joules into
//! money. Prices are approximate residential rates in each country's own
//! currency per kWh (2024–2025 averages, taxes included where they are
//! normally billed) so the tab prices consumption without pretending every
//! country bills in euros. They are display-only defaults, not billing
//! data.

use super::units::NOT_AVAILABLE;

/// One country's residential electricity price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tariff {
    /// ISO-3166-ish selector code shown in the tab (`NL`, `DE`, ...).
    pub code: &'static str,
    /// Display name.
    pub country: &'static str,
    /// ISO-4217 code of the country's native currency (`EUR`, `USD`, ...).
    pub currency: &'static str,
    /// Display symbol for that currency (`€`, `$`, `CHF`, ...).
    pub symbol: &'static str,
    /// Approximate residential price in the native currency per kWh.
    pub price_per_kwh: f64,
}

impl Tariff {
    /// Cost in this country's native currency of `joules` at its price;
    /// `None` while the energy is missing or unusable.
    pub fn cost(&self, joules: Option<f64>) -> Option<f64> {
        let joules = joules?;
        if !joules.is_finite() || joules < 0.0 || !self.price_per_kwh.is_finite() {
            return None;
        }
        Some(joules / 3_600_000.0 * self.price_per_kwh)
    }

    /// An amount in the native currency (`€1.23`, `CHF 0.45`, `kr 2.40`).
    /// Alphabetic symbols get a separating space; amounts below 0.10 keep
    /// three decimals so a cheap window never reads as zero.
    pub fn format_money(&self, amount: f64) -> String {
        if !amount.is_finite() {
            return NOT_AVAILABLE.to_owned();
        }
        let gap = if !self.symbol.is_empty() && self.symbol.chars().all(char::is_alphabetic) {
            " "
        } else {
            ""
        };
        let decimals: usize = if amount > 0.0 && amount < 0.10 { 3 } else { 2 };
        let text = format!("{amount:.decimals$}");
        format!("{}{gap}{text}", self.symbol)
    }

    /// Price text (`€0.30/kWh`).
    pub fn format_price(&self) -> String {
        format!("{}/kWh", self.format_money(self.price_per_kwh))
    }

    /// Cost text (`€1.23`); `n/a` while the energy is missing.
    pub fn format_cost(&self, joules: Option<f64>) -> String {
        self.cost(joules)
            .map_or_else(|| NOT_AVAILABLE.to_owned(), |cost| self.format_money(cost))
    }
}

/// Selectable countries and their display-only tariffs, in a stable order.
/// The first entry is the default (`NL`).
pub const TARIFFS: &[Tariff] = &[
    Tariff {
        code: "NL",
        country: "Netherlands",
        currency: "EUR",
        symbol: "€",
        price_per_kwh: 0.30,
    },
    Tariff {
        code: "US",
        country: "United States",
        currency: "USD",
        symbol: "$",
        price_per_kwh: 0.18,
    },
    Tariff {
        code: "CA",
        country: "Canada",
        currency: "CAD",
        symbol: "C$",
        price_per_kwh: 0.19,
    },
    Tariff {
        code: "GB",
        country: "United Kingdom",
        currency: "GBP",
        symbol: "£",
        price_per_kwh: 0.27,
    },
    Tariff {
        code: "DE",
        country: "Germany",
        currency: "EUR",
        symbol: "€",
        price_per_kwh: 0.38,
    },
    Tariff {
        code: "FR",
        country: "France",
        currency: "EUR",
        symbol: "€",
        price_per_kwh: 0.27,
    },
    Tariff {
        code: "IT",
        country: "Italy",
        currency: "EUR",
        symbol: "€",
        price_per_kwh: 0.31,
    },
    Tariff {
        code: "ES",
        country: "Spain",
        currency: "EUR",
        symbol: "€",
        price_per_kwh: 0.28,
    },
    Tariff {
        code: "SE",
        country: "Sweden",
        currency: "SEK",
        symbol: "kr",
        price_per_kwh: 2.40,
    },
    Tariff {
        code: "NO",
        country: "Norway",
        currency: "NOK",
        symbol: "kr",
        price_per_kwh: 1.75,
    },
    Tariff {
        code: "PL",
        country: "Poland",
        currency: "PLN",
        symbol: "zł",
        price_per_kwh: 1.00,
    },
    Tariff {
        code: "CH",
        country: "Switzerland",
        currency: "CHF",
        symbol: "CHF",
        price_per_kwh: 0.28,
    },
    Tariff {
        code: "AU",
        country: "Australia",
        currency: "AUD",
        symbol: "A$",
        price_per_kwh: 0.40,
    },
    Tariff {
        code: "JP",
        country: "Japan",
        currency: "JPY",
        symbol: "¥",
        price_per_kwh: 37.0,
    },
    Tariff {
        code: "KR",
        country: "South Korea",
        currency: "KRW",
        symbol: "₩",
        price_per_kwh: 190.0,
    },
    Tariff {
        code: "IN",
        country: "India",
        currency: "INR",
        symbol: "₹",
        price_per_kwh: 7.00,
    },
    Tariff {
        code: "BR",
        country: "Brazil",
        currency: "BRL",
        symbol: "R$",
        price_per_kwh: 1.10,
    },
    Tariff {
        code: "ZA",
        country: "South Africa",
        currency: "ZAR",
        symbol: "R",
        price_per_kwh: 2.55,
    },
    Tariff {
        code: "SG",
        country: "Singapore",
        currency: "SGD",
        symbol: "S$",
        price_per_kwh: 0.38,
    },
    Tariff {
        code: "AE",
        country: "UAE",
        currency: "AED",
        symbol: "AED",
        price_per_kwh: 0.32,
    },
];

/// Default tariff selector: the first entry (Netherlands, priced in euros).
pub const DEFAULT_TARIFF: usize = 0;

/// Tariff at `index`, wrapping around the table.
pub fn tariff(index: usize) -> Tariff {
    TARIFFS[index % TARIFFS.len()]
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
        let nl = tariff(DEFAULT_TARIFF);
        assert_eq!(nl.cost(Some(3_600_000.0)), Some(0.30));
        assert_eq!(nl.format_cost(Some(3_600_000.0)), "€0.30");
    }

    #[test]
    fn missing_or_bad_energy_has_no_cost() {
        let nl = tariff(DEFAULT_TARIFF);
        assert_eq!(nl.cost(None), None);
        assert_eq!(nl.format_cost(None), "n/a");
        assert_eq!(nl.cost(Some(f64::NAN)), None);
        assert_eq!(nl.cost(Some(-1.0)), None);
    }

    #[test]
    fn every_tariff_has_a_positive_price_and_currency() {
        assert!(!TARIFFS.is_empty());
        for tariff in TARIFFS {
            assert!(tariff.price_per_kwh > 0.0, "{}", tariff.code);
            assert_eq!(tariff.currency.len(), 3, "{}", tariff.code);
            assert!(!tariff.symbol.is_empty(), "{}", tariff.code);
        }
        assert_eq!(tariff(TARIFFS.len()).code, "NL", "index wraps");
    }

    #[test]
    fn the_default_is_the_netherlands_in_euros() {
        let nl = tariff(DEFAULT_TARIFF);
        assert_eq!(nl.code, "NL");
        assert_eq!(nl.country, "Netherlands");
        assert_eq!(nl.currency, "EUR");
        assert_eq!(nl.symbol, "€");
    }

    #[test]
    fn each_country_prices_in_its_native_currency() {
        let by_code = |code: &str| {
            TARIFFS
                .iter()
                .find(|tariff| tariff.code == code)
                .copied()
                .unwrap_or_else(|| panic!("{code} tariff"))
        };
        assert_eq!(by_code("US").format_price(), "$0.18/kWh");
        assert_eq!(by_code("CA").format_price(), "C$0.19/kWh");
        assert_eq!(by_code("GB").format_price(), "£0.27/kWh");
        assert_eq!(by_code("SE").format_price(), "kr 2.40/kWh");
        assert_eq!(by_code("PL").format_price(), "zł 1.00/kWh");
        assert_eq!(by_code("CH").format_price(), "CHF 0.28/kWh");
        assert_eq!(by_code("JP").format_price(), "¥37.00/kWh");
        assert_eq!(by_code("IN").format_price(), "₹7.00/kWh");
        assert_eq!(by_code("ZA").format_price(), "R 2.55/kWh");
        assert_eq!(by_code("AE").format_price(), "AED 0.32/kWh");
    }

    #[test]
    fn small_costs_keep_three_decimals() {
        let nl = tariff(DEFAULT_TARIFF);
        // 120 Wh at €0.30/kWh is €0.036.
        assert_eq!(nl.format_cost(Some(120.0 * 3_600.0)), "€0.036");
    }
}
