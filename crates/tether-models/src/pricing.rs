//! Pricing tables. Prices live in `pricing.toml`, not in code.
//!
//! The table built into the binary is the `pricing.toml` beside this crate.
//! [`Pricing::from_toml`] loads another, for prices that have moved or a
//! model the built-in table does not know.

use std::collections::HashMap;

use serde::Deserialize;
use tether_types::{Price, Usage};

use crate::ModelError;

const BUILTIN: &str = include_str!("../pricing.toml");

/// Dollars per million tokens for one model.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPrice {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    version: String,
    cache_read_multiplier: f64,
    cache_write_multiplier: f64,
    #[serde(default)]
    models: HashMap<String, ModelPrice>,
}

impl Pricing {
    /// The table shipped with this build.
    pub fn builtin() -> Self {
        // The file is part of the crate and is parsed by a test, so this
        // cannot fail in a build that passed its tests.
        Self::from_toml(BUILTIN).unwrap_or_else(|_| Self {
            version: "unavailable".into(),
            cache_read_multiplier: 0.0,
            cache_write_multiplier: 0.0,
            models: HashMap::new(),
        })
    }

    pub fn from_toml(text: &str) -> Result<Self, ModelError> {
        toml::from_str(text).map_err(|e| ModelError::Provider(format!("pricing table: {e}")))
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// What `usage` costs on `model`, or `None` if the table has no row for it.
    pub fn price(&self, model: &str, usage: &Usage) -> Option<Price> {
        let row = self.models.get(model)?;
        let cache_read = row
            .cache_read
            .unwrap_or(row.input * self.cache_read_multiplier);
        let cache_write = row
            .cache_write
            .unwrap_or(row.input * self.cache_write_multiplier);
        let per_million = usage.input_tokens as f64 * row.input
            + usage.output_tokens as f64 * row.output
            + usage.cache_read_tokens as f64 * cache_read
            + usage.cache_write_tokens as f64 * cache_write;
        Some(Price {
            usd: per_million / 1_000_000.0,
            table: self.version.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        }
    }

    #[test]
    fn builtin_table_parses_and_prices_a_call() {
        let pricing = Pricing::from_toml(BUILTIN).expect("built-in pricing.toml");
        assert_eq!(pricing, Pricing::builtin());
        assert_eq!(pricing.version(), "2026-11-03");

        // 1M in at $4, 1M out at $20.
        let price = pricing
            .price("claude-opus-5-5", &usage(1_000_000, 1_000_000, 0, 0))
            .expect("price");
        assert_eq!((price.usd, price.table.as_str()), (24.0, "2026-11-03"));

        let small = pricing
            .price("claude-opus-5-5", &usage(1200, 300, 0, 0))
            .expect("price");
        assert!((small.usd - 0.0108).abs() < 1e-12, "{}", small.usd);
    }

    #[test]
    fn cache_tokens_use_the_row_price_or_the_multiplier() {
        let pricing = Pricing::builtin();
        // An explicit cache read price: $0.20 per million.
        let explicit = pricing
            .price("claude-opus-5-5", &usage(0, 0, 1_000_000, 0))
            .expect("price");
        assert!((explicit.usd - 0.20).abs() < 1e-12);
        // No explicit write price: 1.25 times the $4 input price.
        let write = pricing
            .price("claude-opus-5-5", &usage(0, 0, 0, 1_000_000))
            .expect("price");
        assert!((write.usd - 5.0).abs() < 1e-12);
        // No explicit read price: 0.1 times the $5 input price.
        let derived = pricing
            .price("claude-opus-5", &usage(0, 0, 1_000_000, 0))
            .expect("price");
        assert!((derived.usd - 0.5).abs() < 1e-12);
    }

    #[test]
    fn an_unknown_model_has_no_price() {
        assert_eq!(
            Pricing::builtin().price("gpt-9", &usage(10, 10, 0, 0)),
            None
        );
    }

    #[test]
    fn a_custom_table_replaces_the_builtin() {
        let pricing = Pricing::from_toml(
            "version = \"mine\"\ncache_read_multiplier = 0.5\ncache_write_multiplier = 2.0\n\n[models.local]\ninput = 1.0\noutput = 2.0\n",
        )
        .expect("pricing");
        let price = pricing
            .price("local", &usage(1_000_000, 0, 1_000_000, 0))
            .expect("price");
        assert_eq!((price.usd, price.table.as_str()), (1.5, "mine"));
        assert!(Pricing::from_toml("version = 1").is_err());
    }
}
