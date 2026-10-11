use super::parse::Request;
use crate::atomic;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelRate {
    pub standard: Price,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<Price>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_prompt: Option<LongPrompt>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LongPrompt {
    pub over_tokens: u64,
    pub standard: Price,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<Price>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PromptTier {
    pub over_tokens: u64,
    pub long: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rates {
    pub source: String,
    pub unit: String,
    pub models: BTreeMap<String, ModelRate>,
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl Cost {
    pub fn total(&self) -> f64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }
}

fn model(
    input: f64,
    output: f64,
    cache_write_5m: f64,
    cache_write_1h: f64,
    cache_read: f64,
) -> ModelRate {
    ModelRate {
        standard: Price {
            input,
            output,
            cache_write_5m,
            cache_write_1h,
            cache_read,
        },
        fast: None,
        long_prompt: None,
    }
}

pub fn seed() -> Rates {
    Rates {
        source: "Anthropic first-party list prices, https://platform.claude.com/docs/en/about-claude/pricing (read 2026-10-10)".into(),
        unit: "USD per million tokens".into(),
        aliases: BTreeMap::new(),
        models: BTreeMap::from([
            ("claude-opus-5-5".into(), model(4.0, 20.0, 5.0, 8.0, 0.2)),
            ("claude-opus-5".into(), model(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-opus-4-8".into(), model(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-sonnet-5".into(), model(2.0, 10.0, 2.5, 4.0, 0.2)),
            ("claude-haiku-4-5-20251001".into(), model(1.0, 5.0, 1.25, 2.0, 0.1)),
            ("claude-sonnet-5-5".into(), model(2.0, 10.0, 2.5, 4.0, 0.1)),
            ("claude-fable-5-1".into(), model(10.0, 50.0, 12.5, 20.0, 0.25)),
            ("claude-haiku-5-5".into(), ModelRate {
                long_prompt: Some(LongPrompt {
                    over_tokens: 100_000,
                    standard: Price {
                        input: 0.5,
                        output: 2.5,
                        cache_write_5m: 0.625,
                        cache_write_1h: 1.0,
                        cache_read: 0.05,
                    },
                    fast: None,
                }),
                ..model(0.1, 0.5, 0.125, 0.2, 0.01)
            }),
        ]),
    }
}

pub fn load_or_seed(dir: &Path) -> Result<Rates> {
    let path = dir.join("rates.json");
    if !path.exists() {
        atomic::write_once(&path, &serde_json::to_vec_pretty(&seed())?)?;
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub fn read_or_seed(dir: &Path) -> Result<Rates> {
    let path = dir.join("rates.json");
    if path.exists() {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    } else {
        Ok(seed())
    }
}

impl Rates {
    pub fn model_rate(&self, name: &str) -> Option<&ModelRate> {
        self.models
            .get(name)
            .or_else(|| self.aliases.get(name).and_then(|id| self.models.get(id)))
    }
    pub fn prompt_tier(&self, request: &Request) -> Option<PromptTier> {
        let long_prompt = self.model_rate(&request.model)?.long_prompt.as_ref()?;
        let length = u128::from(request.input)
            + u128::from(request.cache_write_5m)
            + u128::from(request.cache_write_1h)
            + u128::from(request.cache_read);
        Some(PromptTier {
            over_tokens: long_prompt.over_tokens,
            long: length > u128::from(long_prompt.over_tokens),
        })
    }

    pub fn price_for(
        &self,
        name: &str,
        speed: Option<&str>,
        tier: Option<PromptTier>,
    ) -> Option<&Price> {
        let model = self.model_rate(name)?;
        let (standard, fast) = if let Some(long_prompt) = &model.long_prompt {
            let tier = tier.filter(|tier| tier.over_tokens == long_prompt.over_tokens)?;
            if tier.long {
                (&long_prompt.standard, long_prompt.fast.as_ref())
            } else {
                (&model.standard, model.fast.as_ref())
            }
        } else {
            (&model.standard, model.fast.as_ref())
        };
        if speed == Some("fast") {
            fast
        } else {
            Some(standard)
        }
    }
}

pub fn edit_alias(dir: &Path, model: &str, target: Option<&str>) -> Result<bool> {
    let path = dir.join("rates.json");
    if !path.exists() {
        atomic::write_once(&path, &serde_json::to_vec_pretty(&seed())?)?;
    }
    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let rates: Rates = serde_json::from_value(raw.clone())?;
    if let Some(target) = target {
        anyhow::ensure!(
            rates.models.contains_key(target),
            "unknown rates model id: {target}"
        );
        let aliases = raw
            .as_object_mut()
            .unwrap()
            .entry("aliases")
            .or_insert_with(|| serde_json::json!({}));
        aliases
            .as_object_mut()
            .unwrap()
            .insert(model.into(), target.into());
    } else {
        let Some(aliases) = raw.get_mut("aliases") else {
            return Ok(false);
        };
        if aliases.as_object_mut().unwrap().remove(model).is_none() {
            return Ok(false);
        }
    }
    atomic::write(&path, &serde_json::to_vec_pretty(&raw)?)?;
    Ok(true)
}

pub fn cost(request: &Request, rates: &Rates) -> Option<Cost> {
    // Known-bad: prefix matching "sonnet" or pricing fast as standard silently
    // invents a rate. Both cases must remain unpriced until explicitly listed.
    let price = rates.price_for(
        &request.model,
        request.speed.as_deref(),
        rates.prompt_tier(request),
    )?;
    let per_million = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    Some(Cost {
        input: per_million(request.input, price.input),
        output: per_million(request.output, price.output),
        cache_write_5m: per_million(request.cache_write_5m, price.cache_write_5m),
        cache_write_1h: per_million(request.cache_write_1h, price.cache_write_1h),
        cache_read: per_million(request.cache_read, price.cache_read),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::parse;
    use serde_json::json;

    fn request(model: &str, speed: Option<&str>) -> Request {
        let line = json!({
            "type":"assistant","timestamp":"2030-01-01T12:00:00Z","sessionId":"sample",
            "requestId":"req-1","message":{"id":"msg-1","model":model,"usage":{
                "input_tokens":1_000_000,"output_tokens":1_000_000,
                "cache_creation_input_tokens":2_000_000,
                "cache_creation":{"ephemeral_5m_input_tokens":1_000_000,"ephemeral_1h_input_tokens":1_000_000},
                "cache_read_input_tokens":1_000_000,"speed":speed}}
        });
        parse::parse(line.to_string().as_bytes(), "sample")
            .unwrap()
            .unwrap()
            .requests
            .remove(0)
    }

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn seed_has_exact_new_model_cards_and_source() {
        // Known-bads: Sonnet 5.5 cache reads at 0.20, or identical Haiku 5.5 cards.
        let rates = seed();
        assert_eq!(
            rates.source,
            "Anthropic first-party list prices, https://platform.claude.com/docs/en/about-claude/pricing (read 2026-10-10)"
        );
        let values = |price: &Price| {
            [
                price.input,
                price.output,
                price.cache_write_5m,
                price.cache_write_1h,
                price.cache_read,
            ]
        };
        let total = |price: &Price| values(price).into_iter().sum::<f64>();
        let haiku = &rates.models["claude-haiku-5-5"];
        assert_eq!(values(&haiku.standard), [0.1, 0.5, 0.125, 0.2, 0.01]);
        assert_eq!(total(&haiku.standard), 0.935);
        let long = haiku.long_prompt.as_ref().unwrap();
        assert_eq!(long.over_tokens, 100_000);
        assert_eq!(values(&long.standard), [0.5, 2.5, 0.625, 1.0, 0.05]);
        assert_eq!(total(&long.standard), 4.675);
        assert!(haiku.fast.is_none() && long.fast.is_none());
        let sonnet = &rates.models["claude-sonnet-5-5"];
        assert_eq!(values(&sonnet.standard), [2.0, 10.0, 2.5, 4.0, 0.1]);
        assert_eq!(total(&sonnet.standard), 18.6);
        assert!(sonnet.fast.is_none() && sonnet.long_prompt.is_none());
        let fable = &rates.models["claude-fable-5-1"];
        assert_eq!(values(&fable.standard), [10.0, 50.0, 12.5, 20.0, 0.25]);
        assert_eq!(total(&fable.standard), 92.75);
        assert!(fable.fast.is_none() && fable.long_prompt.is_none());
    }

    #[test]
    fn prompt_threshold_is_exclusive() {
        // Known-bad: >= switches the exactly-100,000-token prompt to the long card.
        let rates = seed();
        let mut row = request("claude-haiku-5-5", None);
        row.input = 1;
        row.output = 0;
        row.cache_write_5m = 0;
        row.cache_write_1h = 0;
        row.cache_read = 99_999;
        assert!(!rates.prompt_tier(&row).unwrap().long);
        close(
            cost(&row, &rates).unwrap().cache_read,
            99_999.0 * 0.01 / 1_000_000.0,
        );
        row.input = 2;
        assert!(rates.prompt_tier(&row).unwrap().long);
        close(
            cost(&row, &rates).unwrap().cache_read,
            99_999.0 * 0.05 / 1_000_000.0,
        );
    }

    #[test]
    fn prompt_length_counts_four_input_kinds_but_not_output() {
        // Known-bads: omitting any input kind, using input alone, or counting output.
        let rates = seed();
        let mut row = request("claude-haiku-5-5", None);
        row.input = 10;
        row.cache_write_5m = 40_000;
        row.cache_write_1h = 30_000;
        row.cache_read = 30_001;
        row.output = 0;
        assert!(rates.prompt_tier(&row).unwrap().long);
        close(
            cost(&row, &rates).unwrap().cache_read,
            30_001.0 * 0.05 / 1_000_000.0,
        );
        row.input = 1_000;
        row.cache_write_5m = 9_000;
        row.cache_write_1h = 10_000;
        row.cache_read = 40_000;
        row.output = 50_000;
        assert!(!rates.prompt_tier(&row).unwrap().long);
        close(
            cost(&row, &rates).unwrap().cache_read,
            40_000.0 * 0.01 / 1_000_000.0,
        );
    }

    #[test]
    fn fast_long_request_requires_long_fast_card() {
        // Known-bads: falling back to long standard or the model's short fast card.
        let mut rates = seed();
        let mut invented = model(1.0, 0.0, 0.0, 0.0, 0.0);
        invented.fast = Some(Price {
            input: 7.0,
            ..invented.standard.clone()
        });
        invented.long_prompt = Some(LongPrompt {
            over_tokens: 100,
            standard: Price {
                input: 9.0,
                ..invented.standard.clone()
            },
            fast: None,
        });
        rates.models.insert("invented-model".into(), invented);
        let mut row = request("invented-model", Some("fast"));
        row.input = 101;
        row.output = 0;
        row.cache_write_5m = 0;
        row.cache_write_1h = 0;
        row.cache_read = 0;
        assert!(cost(&row, &rates).is_none());
        rates
            .models
            .get_mut("invented-model")
            .unwrap()
            .long_prompt
            .as_mut()
            .unwrap()
            .fast = Some(Price {
            input: 11.0,
            ..model(0.0, 0.0, 0.0, 0.0, 0.0).standard
        });
        close(
            cost(&row, &rates).unwrap().total(),
            101.0 * 11.0 / 1_000_000.0,
        );
    }

    #[test]
    fn exact_rate_math_uses_separate_five_minute_and_one_hour_writes() {
        // Known-bad: pricing both cache-write buckets at one rate, or adding
        // their `cache_creation_input_tokens` total a second time.
        let costs = cost(&request("claude-opus-5-5", Some("standard")), &seed()).unwrap();
        assert_eq!(costs.input, 4.00);
        assert_eq!(costs.output, 20.00);
        assert_eq!(costs.cache_write_5m, 5.00);
        assert_eq!(costs.cache_write_1h, 8.00);
        assert_eq!(costs.cache_read, 0.20);
        assert_eq!(format!("{:.2}", costs.total()), "37.20");
    }

    #[test]
    fn unknown_model_and_fast_without_rate_are_unpriced() {
        // Known-bad: guessed model prefixes and standard-price fallback for fast.
        assert!(cost(&request("claude-sonnet-5-extra", None), &seed()).is_none());
        assert!(cost(&request("claude-sonnet-5", Some("fast")), &seed()).is_none());
    }

    #[test]
    fn existing_rates_file_is_never_overwritten() {
        // Known-bad: reseeding on each invocation destroys a user's edits.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rates.json");
        let mut rates = seed();
        rates
            .models
            .get_mut("claude-sonnet-5")
            .unwrap()
            .standard
            .input = 9.0;
        let bytes = serde_json::to_vec_pretty(&rates).unwrap();
        fs::write(&path, &bytes).unwrap();
        let loaded = load_or_seed(tmp.path()).unwrap();
        assert_eq!(loaded.models["claude-sonnet-5"].standard.input, 9.0);
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[cfg(test)]
mod alias_tests {
    use super::*;
    #[test]
    fn exact_alias_prices_only_the_named_gateway_model() {
        // Known-bad: normalizing dots, stripping prefixes, or accepting a dangling alias.
        let mut rates = seed();
        assert!(rates.model_rate("acme/claude-x.5").is_none());
        rates
            .aliases
            .insert("acme/claude-x.5".into(), "claude-opus-5".into());
        assert_eq!(
            rates.model_rate("acme/claude-x.5").unwrap().standard.input,
            5.0
        );
        assert!(rates.model_rate("ACME/claude-x.5").is_none());
        assert!(rates.model_rate("acme/claude-opus-5.5").is_none()); // Known-bad: stripping a prefix and normalizing the dot to a dash.
        rates.aliases.insert("bad".into(), "missing".into());
        assert!(rates.model_rate("bad").is_none());
        let tmp = tempfile::tempdir().unwrap();
        assert!(edit_alias(tmp.path(), "another", Some("missing")).is_err());
        assert!(
            !tmp.path().join("rates.json").exists()
                || !read_or_seed(tmp.path())
                    .unwrap()
                    .aliases
                    .contains_key("another")
        );
    }
}

#[cfg(test)]
mod alias_file_tests {
    use super::*;
    #[test]
    fn malformed_rates_file_is_left_unchanged() {
        // Known-bad: a failed alias edit replacing malformed user rates with the seed.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rates.json");
        fs::write(&path, b"{bad").unwrap();
        assert!(edit_alias(tmp.path(), "acme/x", Some("claude-opus-5")).is_err());
        assert_eq!(fs::read(path).unwrap(), b"{bad");
    }
    #[test]
    fn alias_edit_preserves_unknown_fields_at_both_depths() {
        // Known-bad: serializing through Rates drops hand-edited fields or the long card.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rates.json");
        let mut raw = serde_json::to_value(seed()).unwrap();
        raw["note"] = serde_json::json!("synthetic note");
        raw["models"]["claude-opus-5"]["standard"]["extra"] = serde_json::json!(42);
        let long_card = raw["models"]["claude-haiku-5-5"]["long_prompt"].clone();
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        edit_alias(tmp.path(), "acme/x", Some("claude-opus-5")).unwrap();
        let set: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(set["aliases"]["acme/x"], "claude-opus-5");
        assert_eq!(set["note"], "synthetic note");
        assert_eq!(set["models"]["claude-opus-5"]["standard"]["extra"], 42);
        assert_eq!(set["models"]["claude-haiku-5-5"]["long_prompt"], long_card);
        edit_alias(tmp.path(), "acme/x", None).unwrap();
        let removed: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(
            removed["aliases"]
                .as_object()
                .unwrap()
                .get("acme/x")
                .is_none()
        );
        assert_eq!(removed["note"], "synthetic note");
        assert_eq!(removed["models"]["claude-opus-5"]["standard"]["extra"], 42);
        assert_eq!(
            removed["models"]["claude-haiku-5-5"]["long_prompt"],
            long_card
        );
    }
    #[test]
    fn incomplete_long_prompt_cards_fail_without_rewriting_rates_file() {
        // Known-bad: a default of zero for missing over_tokens accepts malformed rates.
        for missing in ["over_tokens", "standard"] {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("rates.json");
            let mut raw = serde_json::to_value(seed()).unwrap();
            raw["models"]["claude-haiku-5-5"]["long_prompt"]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            let bytes = serde_json::to_vec_pretty(&raw).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(load_or_seed(tmp.path()).is_err(), "missing {missing}");
            assert!(edit_alias(tmp.path(), "acme/tiny", Some("claude-haiku-5-5")).is_err());
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
    #[test]
    fn removing_missing_alias_is_a_noop() {
        // Known-bad: claiming an absent alias was updated and rewriting rates.json.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rates.json");
        let data = serde_json::to_vec(&seed()).unwrap();
        fs::write(&path, &data).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!edit_alias(tmp.path(), "acme/missing", None).unwrap());
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
}
