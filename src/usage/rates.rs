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
    }
}

pub fn seed() -> Rates {
    Rates {
        source: "Anthropic first-party list prices, Claude Code 2.1.280 bundled API reference (2026-06-24)".into(),
        unit: "USD per million tokens".into(),
        aliases: BTreeMap::new(),
        models: BTreeMap::from([
            ("claude-opus-5-5".into(), model(4.0, 20.0, 5.0, 8.0, 0.2)),
            ("claude-opus-5".into(), model(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-opus-4-8".into(), model(5.0, 25.0, 6.25, 10.0, 0.5)),
            ("claude-sonnet-5".into(), model(2.0, 10.0, 2.5, 4.0, 0.2)),
            ("claude-haiku-4-5-20251001".into(), model(1.0, 5.0, 1.25, 2.0, 0.1)),
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
    pub fn price_for(&self, name: &str, speed: Option<&str>) -> Option<&Price> {
        let model = self.model_rate(name)?;
        if speed == Some("fast") {
            model.fast.as_ref()
        } else {
            Some(&model.standard)
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
    let price = rates.price_for(&request.model, request.speed.as_deref())?;
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
        // Known-bad: serializing through Rates drops hand-edited fields.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rates.json");
        let mut raw = serde_json::to_value(seed()).unwrap();
        raw["note"] = serde_json::json!("synthetic note");
        raw["models"]["claude-opus-5"]["standard"]["extra"] = serde_json::json!(42);
        fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        edit_alias(tmp.path(), "acme/x", Some("claude-opus-5")).unwrap();
        let set: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(set["aliases"]["acme/x"], "claude-opus-5");
        assert_eq!(set["note"], "synthetic note");
        assert_eq!(set["models"]["claude-opus-5"]["standard"]["extra"], 42);
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
