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

pub fn cost(request: &Request, rates: &Rates) -> Option<Cost> {
    // Known-bad: prefix matching "sonnet" or pricing fast as standard silently
    // invents a rate. Both cases must remain unpriced until explicitly listed.
    let model = rates.models.get(&request.model)?;
    let price = if request.speed.as_deref() == Some("fast") {
        model.fast.as_ref()?
    } else {
        &model.standard
    };
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
