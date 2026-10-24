//! The unified listener's routing table: model aliases, explicit `provider/model` references, the
//! operator's prefix routes and the built-in model-name patterns. Pure and shared, so configuration
//! validation and the proxy agree on what a reference means; the proxy adds what needs a live request
//! (path prefixes, path families, header hints and the live model lists).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{RouteRule, UnifiedConfig};
use crate::types::Backend;

/// Longest alias name or model reference accepted from the configuration.
pub const MAX_MODEL_REF_LEN: usize = 256;

/// A model as a client or the configuration spells it. `anthropic/claude-sonnet-5-5` names the provider;
/// `gemma4:12b-mlx` leaves it to the resolver. Only the five backend ids count as a provider prefix, so LM
/// Studio's `publisher/model` ids such as `qwen/qwen3.6-27b` are plain model names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub backend: Option<Backend>,
    pub model: String,
}

impl ModelRef {
    pub fn parse(s: &str) -> Result<ModelRef, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty model reference".to_string());
        }
        if s.len() > MAX_MODEL_REF_LEN {
            return Err(format!(
                "model reference is longer than {MAX_MODEL_REF_LEN} bytes"
            ));
        }
        if s.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(format!(
                "model reference {s:?} contains whitespace or control characters"
            ));
        }
        if let Some((head, rest)) = s.split_once('/')
            && let Some(backend) = provider_id(head)
        {
            if rest.is_empty() {
                return Err(format!("`{s}` names a provider but no model"));
            }
            return Ok(ModelRef {
                backend: Some(backend),
                model: rest.to_string(),
            });
        }
        Ok(ModelRef {
            backend: None,
            model: s.to_string(),
        })
    }
}

/// The backend whose exact id this is, if any. Deliberately stricter than `Backend::from_str`, which also
/// takes human spellings such as `lm studio`.
pub fn provider_id(s: &str) -> Option<Backend> {
    Backend::ALL
        .into_iter()
        .find(|b| b.as_str().eq_ignore_ascii_case(s))
}

/// How the unified listener chose a request's backend; stored on the record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutedBy {
    /// A per-backend listener: the listener is the choice.
    Listener,
    /// An explicit `/anthropic/…`-style path prefix.
    Prefix,
    /// A provider-specific path family (`/api/v0/*`, `/api/*`, `/v1beta/*`).
    Path,
    /// A configured alias.
    Alias,
    /// The model is in a local backend's live model list.
    ModelList,
    /// A `provider/model` spelling.
    ProviderPrefix,
    /// A configured `[[unified.routes]]` prefix.
    Route,
    /// A built-in provider pattern such as `claude-*`.
    ModelPattern,
    /// A provider-specific request header on a model-less request.
    Header,
    /// Nothing else decided: the configured default backend.
    Default,
}

impl RoutedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            RoutedBy::Listener => "listener",
            RoutedBy::Prefix => "prefix",
            RoutedBy::Path => "path",
            RoutedBy::Alias => "alias",
            RoutedBy::ModelList => "model_list",
            RoutedBy::ProviderPrefix => "provider_prefix",
            RoutedBy::Route => "route",
            RoutedBy::ModelPattern => "model_pattern",
            RoutedBy::Header => "header",
            RoutedBy::Default => "default",
        }
    }
}

/// The configured part of routing: hot-reloadable, pure data.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingTable {
    pub default_backend: Backend,
    /// Alias names, lower-cased, to what they stand for.
    pub aliases: BTreeMap<String, ModelRef>,
    pub routes: Vec<RouteRule>,
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self {
            default_backend: Backend::LmStudio,
            aliases: BTreeMap::new(),
            routes: Vec::new(),
        }
    }
}

impl RoutingTable {
    /// Builds the table; an alias whose target does not parse is dropped (validation reports it first).
    pub fn from_config(cfg: &UnifiedConfig) -> Self {
        let aliases = cfg
            .aliases
            .iter()
            .filter_map(|(name, target)| {
                ModelRef::parse(target)
                    .ok()
                    .map(|target| (name.trim().to_ascii_lowercase(), target))
            })
            .collect();
        Self {
            default_backend: cfg.default_backend,
            aliases,
            routes: cfg.routes.clone(),
        }
    }

    /// The alias `model` names, case-insensitively.
    pub fn alias(&self, model: &str) -> Option<&ModelRef> {
        self.aliases.get(&model.trim().to_ascii_lowercase())
    }

    /// The configured route with the longest prefix of `model`, case-insensitively.
    pub fn route(&self, model: &str) -> Option<Backend> {
        let lower = model.to_ascii_lowercase();
        self.routes
            .iter()
            .filter(|r| lower.starts_with(&r.model_prefix.to_ascii_lowercase()))
            .max_by_key(|r| r.model_prefix.len())
            .map(|r| r.backend)
    }
}

/// The ids each cloud provider publishes, so `claude-*`, `gemini-*` and `gpt-*` need no configuration.
pub fn builtin_pattern(model: &str) -> Option<Backend> {
    let m = model.trim().to_ascii_lowercase();
    let m = m.strip_prefix("models/").unwrap_or(&m);
    if m.starts_with("claude-") {
        Some(Backend::Anthropic)
    } else if m.starts_with("gemini-") {
        Some(Backend::Gemini)
    } else if m.starts_with("gpt-")
        || m.starts_with("chatgpt-")
        || m.starts_with("text-embedding-3")
        || m.starts_with("text-embedding-ada")
        || is_o_series(m)
    {
        Some(Backend::OpenAi)
    } else {
        None
    }
}

/// `o1`, `o3`, `o4` and their `-mini`/`-pro` variants.
fn is_o_series(m: &str) -> bool {
    let mut chars = m.chars();
    chars.next() == Some('o')
        && matches!(chars.next(), Some('1' | '3' | '4'))
        && matches!(chars.next(), None | Some('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_refs_take_only_the_five_backend_ids_as_providers() {
        assert_eq!(
            ModelRef::parse("anthropic/claude-sonnet-5-5"),
            Ok(ModelRef {
                backend: Some(Backend::Anthropic),
                model: "claude-sonnet-5-5".into()
            })
        );
        assert_eq!(
            ModelRef::parse(" Ollama/gemma4:12b-mlx "),
            Ok(ModelRef {
                backend: Some(Backend::Ollama),
                model: "gemma4:12b-mlx".into()
            })
        );
        assert_eq!(
            ModelRef::parse("qwen/qwen3.6-27b"),
            Ok(ModelRef {
                backend: None,
                model: "qwen/qwen3.6-27b".into()
            }),
            "an LM Studio publisher is not a provider"
        );
        assert_eq!(
            ModelRef::parse("gemma4:12b-mlx"),
            Ok(ModelRef {
                backend: None,
                model: "gemma4:12b-mlx".into()
            })
        );
        assert!(ModelRef::parse("").is_err());
        assert!(ModelRef::parse("openai/").is_err());
        assert!(ModelRef::parse("two words").is_err());
        assert!(ModelRef::parse("evil\u{1b}").is_err());
        assert!(ModelRef::parse(&"m".repeat(MAX_MODEL_REF_LEN + 1)).is_err());
    }

    #[test]
    fn builtin_patterns_cover_the_three_providers_and_nothing_else() {
        for (model, expected) in [
            ("claude-sonnet-5-5", Some(Backend::Anthropic)),
            ("Claude-Opus-5", Some(Backend::Anthropic)),
            ("gemini-3.1-pro-preview", Some(Backend::Gemini)),
            ("models/gemini-3-flash", Some(Backend::Gemini)),
            ("gemini-embedding-2", Some(Backend::Gemini)),
            ("gpt-5.5", Some(Backend::OpenAi)),
            ("chatgpt-4o-latest", Some(Backend::OpenAi)),
            ("o3-mini", Some(Backend::OpenAi)),
            ("o1", Some(Backend::OpenAi)),
            ("o4-mini-high", Some(Backend::OpenAi)),
            ("text-embedding-3-large", Some(Backend::OpenAi)),
            ("gpt-oss-20b", Some(Backend::OpenAi)),
            ("openai/gpt-oss-20b", None),
            ("gemma4:12b-mlx", None),
            ("qwen/qwen3.6-27b", None),
            ("o5", None),
            ("ollama", None),
            ("", None),
        ] {
            assert_eq!(builtin_pattern(model), expected, "{model}");
        }
    }

    #[test]
    fn the_table_resolves_aliases_case_insensitively_and_routes_by_longest_prefix() {
        let cfg = UnifiedConfig {
            aliases: BTreeMap::from([
                ("Fast".to_string(), "ollama/gemma4:12b-mlx".to_string()),
                ("smart".to_string(), "claude-opus-5-5".to_string()),
                ("broken".to_string(), "openai/".to_string()),
            ]),
            routes: vec![
                RouteRule {
                    model_prefix: "mistral-".into(),
                    backend: Backend::Ollama,
                },
                RouteRule {
                    model_prefix: "mistral-large".into(),
                    backend: Backend::LmStudio,
                },
            ],
            ..UnifiedConfig::default()
        };
        let table = RoutingTable::from_config(&cfg);
        assert_eq!(table.default_backend, Backend::LmStudio);
        assert_eq!(
            table.alias("FAST"),
            Some(&ModelRef {
                backend: Some(Backend::Ollama),
                model: "gemma4:12b-mlx".into()
            })
        );
        assert_eq!(
            table.alias("smart").map(|m| m.backend),
            Some(None),
            "a bare target leaves the provider to the resolver"
        );
        assert_eq!(
            table.alias("broken"),
            None,
            "unparsable targets are dropped"
        );
        assert_eq!(table.alias("nope"), None);
        assert_eq!(table.route("mistral-small"), Some(Backend::Ollama));
        assert_eq!(table.route("Mistral-Large-3"), Some(Backend::LmStudio));
        assert_eq!(table.route("llama-4"), None);
        assert_eq!(RoutingTable::default().alias("x"), None);
    }
}
