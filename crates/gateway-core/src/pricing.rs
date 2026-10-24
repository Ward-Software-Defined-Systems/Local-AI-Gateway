//! Hypothetical frontier-API cost: what the session's tokens would have cost at the list prices of the
//! models in `pricing.toml`, compiled into the binary. The user may override or add rates under
//! `[pricing.providers.<provider>.models.<key>]` in `config.toml`; an override replaces the key wherever
//! it lived, so a model never has two rates (LMS-Monitor's merge). Prices are flat per million tokens: no
//! caching discounts, batch rates or long-context tiers. Ported from the two monitors' `pricing.rs`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The cost table's columns, in display order.
pub const FRONTIER_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "gemini-3-1-pro",
];

const BAKED_PRICING: &str = include_str!("../../../pricing.toml");

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    #[serde(default)]
    models: BTreeMap<String, ModelPricing>,
}

/// `pricing.toml`, and the `[pricing]` table of `config.toml`, share this shape.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PricingFile {
    #[serde(default)]
    providers: BTreeMap<String, Provider>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PriceEntry {
    pub provider: String,
    pub price: ModelPricing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PricingTable {
    by_model: BTreeMap<String, PriceEntry>,
}

impl PricingTable {
    /// The compiled-in table alone.
    pub fn baked() -> PricingTable {
        let file: PricingFile = toml::from_str(BAKED_PRICING).expect("pricing.toml is valid");
        let mut table = PricingTable {
            by_model: BTreeMap::new(),
        };
        table.absorb(file);
        table
    }

    /// The compiled-in table with the user's `[pricing]` overrides applied. A malformed override is an
    /// error, like any other configuration mistake.
    pub fn load(overrides: Option<&toml::Table>) -> Result<PricingTable> {
        let mut table = Self::baked();
        if let Some(overrides) = overrides {
            let file: PricingFile = overrides
                .clone()
                .try_into()
                .context("[pricing] in config.toml: expected [pricing.providers.<provider>.models.<key>] tables with input_per_mtok_usd and output_per_mtok_usd")?;
            table.absorb(file);
        }
        Ok(table)
    }

    fn absorb(&mut self, file: PricingFile) {
        for (provider, entries) in file.providers {
            for (key, price) in entries.models {
                self.by_model.insert(
                    key,
                    PriceEntry {
                        provider: provider.clone(),
                        price,
                    },
                );
            }
        }
    }

    pub fn get(&self, model_key: &str) -> Option<ModelPricing> {
        self.by_model.get(model_key).map(|e| e.price)
    }

    /// The rate for a model id as a provider spells it (`claude-opus-5-5-20260101`, `gemini-3.1-pro-preview`,
    /// `openai/gpt-5.5`): the id is normalised to the table's key style and the longest key that prefixes
    /// it at a `-`, `:`, `@` or end boundary wins, so a dated snapshot or a `-preview` suffix still prices.
    pub fn price_for_model(&self, model: &str) -> Option<(&str, ModelPricing)> {
        let id = normalise_model_id(model);
        self.by_model
            .iter()
            .filter(|(key, _)| {
                id.strip_prefix(key.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', ':', '@']))
            })
            .max_by_key(|(key, _)| key.len())
            .map(|(key, entry)| (key.as_str(), entry.price))
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &PriceEntry)> {
        self.by_model.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Frontier columns without a rate; each shows `(no rate)` and is worth a startup warning.
    pub fn missing_frontier(&self) -> Vec<&'static str> {
        FRONTIER_MODELS
            .iter()
            .copied()
            .filter(|key| !self.by_model.contains_key(*key))
            .collect()
    }

    /// The cost table: one entry per frontier column that has a rate.
    pub fn costs(&self, prompt_tokens: u64, gen_tokens: u64) -> BTreeMap<String, HypotheticalCost> {
        FRONTIER_MODELS
            .iter()
            .filter_map(|key| {
                self.get(key).map(|price| {
                    (
                        (*key).to_string(),
                        hypothetical_cost(prompt_tokens, gen_tokens, &price),
                    )
                })
            })
            .collect()
    }

    /// Total USD per frontier column, the shape frozen into each request record.
    pub fn totals(&self, prompt_tokens: u64, gen_tokens: u64) -> BTreeMap<String, f64> {
        self.costs(prompt_tokens, gen_tokens)
            .into_iter()
            .map(|(k, c)| (k, c.total_usd))
            .collect()
    }
}

/// Lower-case, `provider/` and Gemini's `models/` prefixes dropped, dots to hyphens: the table's key style.
fn normalise_model_id(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let stripped = crate::types::Backend::ALL
        .iter()
        .find_map(|b| lower.strip_prefix(&format!("{}/", b.as_str())))
        .unwrap_or(&lower);
    let stripped = stripped.strip_prefix("models/").unwrap_or(stripped);
    stripped.replace('.', "-")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct HypotheticalCost {
    pub input_usd: f64,
    pub output_usd: f64,
    pub total_usd: f64,
}

pub fn hypothetical_cost(
    prompt_tokens: u64,
    gen_tokens: u64,
    price: &ModelPricing,
) -> HypotheticalCost {
    let input_usd = prompt_tokens as f64 / 1_000_000.0 * price.input_per_mtok_usd;
    let output_usd = gen_tokens as f64 / 1_000_000.0 * price.output_per_mtok_usd;
    HypotheticalCost {
        input_usd,
        output_usd,
        total_usd: input_usd + output_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_all_frontier_models() {
        let table = PricingTable::baked();
        assert!(
            table.missing_frontier().is_empty(),
            "{:?}",
            table.missing_frontier()
        );
    }

    #[test]
    fn defaults_match_pricing_toml() {
        let table = PricingTable::baked();
        for (key, input, output) in [
            ("claude-fable-5-1", 10.00, 50.00),
            ("claude-fable-5", 10.00, 50.00),
            ("claude-opus-5-5", 4.00, 20.00),
            ("claude-opus-5", 5.00, 25.00),
            ("claude-opus-4-8", 5.00, 25.00),
            ("gemini-3-1-pro", 2.00, 12.00),
        ] {
            let p = table.get(key).unwrap();
            assert_eq!(
                (p.input_per_mtok_usd, p.output_per_mtok_usd),
                (input, output),
                "{key}"
            );
        }
        assert_eq!(table.by_model["gemini-3-1-pro"].provider, "google");
    }

    #[test]
    fn one_million_in_one_million_out_matches_rates() {
        let table = PricingTable::baked();
        let opus = table.get("claude-opus-4-8").unwrap();
        let cost = hypothetical_cost(1_000_000, 1_000_000, &opus);
        assert_eq!(cost.input_usd, 5.00);
        assert_eq!(cost.output_usd, 25.00);
        assert_eq!(cost.total_usd, 30.00);
        let costs = table.costs(1_000_000, 1_000_000);
        assert_eq!(costs.len(), FRONTIER_MODELS.len());
        assert_eq!(costs["gemini-3-1-pro"].total_usd, 14.00);
        assert_eq!(
            table.totals(1_000_000, 1_000_000)["claude-fable-5-1"],
            60.00
        );
    }

    #[test]
    fn an_override_replaces_the_key_whatever_provider_held_it() {
        let cfg = crate::config::Config::parse(
            "[pricing.providers.google.models.gemini-3-1-pro]\ninput_per_mtok_usd = 4.0\noutput_per_mtok_usd = 18.0\n\
             [pricing.providers.mine.models.claude-opus-5]\ninput_per_mtok_usd = 1.0\noutput_per_mtok_usd = 2.0\n\
             [pricing.providers.mine.models.local-model]\ninput_per_mtok_usd = 0.0\noutput_per_mtok_usd = 0.0\n",
        )
        .unwrap();
        let table = PricingTable::load(cfg.pricing.as_ref()).unwrap();
        assert_eq!(table.get("gemini-3-1-pro").unwrap().input_per_mtok_usd, 4.0);
        let opus = &table.by_model["claude-opus-5"];
        assert_eq!(
            opus.provider, "mine",
            "the key moved to the override's provider"
        );
        assert_eq!(opus.price.output_per_mtok_usd, 2.0);
        assert_eq!(table.get("local-model").unwrap().input_per_mtok_usd, 0.0);
        assert_eq!(
            table.entries().count(),
            PricingTable::baked().entries().count() + 1
        );
        assert!(table.missing_frontier().is_empty());
    }

    #[test]
    fn provider_model_ids_price_by_their_longest_table_key() {
        let table = PricingTable::baked();
        for (model, key) in [
            ("claude-opus-5-5", "claude-opus-5-5"),
            ("claude-opus-5-5-20260101", "claude-opus-5-5"),
            ("Claude-Opus-5@2026", "claude-opus-5"),
            ("anthropic/claude-fable-5-1", "claude-fable-5-1"),
            ("gemini-3.1-pro-preview", "gemini-3-1-pro"),
            ("models/gemini-3.1-pro", "gemini-3-1-pro"),
            ("gpt-5.5", "gpt-5-5"),
            ("openai/gpt-5.5-pro", "gpt-5-5-pro"),
            ("gpt-5-mini-2026-01-01", "gpt-5-mini"),
            ("o3-mini", "o3-mini"),
            ("text-embedding-3-small", "text-embedding-3-small"),
        ] {
            let (found, price) = table
                .price_for_model(model)
                .unwrap_or_else(|| panic!("{model} has no price"));
            assert_eq!(found, key, "{model}");
            assert!(price.input_per_mtok_usd >= 0.0);
        }
        assert_eq!(
            table
                .price_for_model("claude-opus-5-5-pro-max")
                .map(|(k, _)| k),
            Some("claude-opus-5-5"),
            "an unknown suffix still prices at the known prefix"
        );
        assert_eq!(
            table.price_for_model("claude-opus-55"),
            None,
            "a key must end at a `-`, `:`, `@` or end boundary of the id"
        );
        assert_eq!(table.price_for_model("qwen/qwen3.6-27b"), None);
        assert_eq!(table.price_for_model(""), None);
    }

    #[test]
    fn a_malformed_override_is_an_error() {
        let cfg = crate::config::Config::parse(
            "[pricing.providers.google.models.gemini-3-1-pro]\ninput_per_mtok_usd = 4.0\n",
        )
        .unwrap();
        let err = PricingTable::load(cfg.pricing.as_ref()).unwrap_err();
        assert!(
            format!("{err:#}").contains("output_per_mtok_usd"),
            "{err:#}"
        );
        let cfg = crate::config::Config::parse("[pricing]\nshow = [\"x\"]\n").unwrap();
        assert!(
            PricingTable::load(cfg.pricing.as_ref()).is_err(),
            "unknown pricing keys are errors"
        );
        assert!(PricingTable::load(None).is_ok());
    }
}
