//! Owner-entered offline billing assumptions. No billing service is contacted.
use super::ledger::Store;
use super::rates::{Cost, Price, Rates};
use crate::atomic;
use anyhow::{Result, bail};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Billing {
    pub version: u32,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileBilling>,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProfileBilling {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<Rate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_ups: Vec<TopUp>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub label: String,
    pub fee_usd: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rate {
    #[serde(default)]
    pub model_prefixes: Vec<String>,
    #[serde(flatten)]
    pub price: RatePrice,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RatePrice {
    Flat { flat: f64 },
    PerType(Price),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopUp {
    pub date: NaiveDate,
    pub usd: f64,
}

pub fn valid_amount(value: f64) -> Result<f64> {
    if value.is_finite() && (0.0..=1_000_000.0).contains(&value) {
        Ok(value)
    } else {
        bail!("amount must be finite and between 0 and 1000000")
    }
}
fn valid_prefix(prefix: &str) -> bool {
    (1..=64).contains(&prefix.len()) && prefix.bytes().all(|b| (32..=126).contains(&b))
}
impl Billing {
    pub fn validate(&self, today: NaiveDate) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported billing version")
        }
        for entry in self.profiles.values() {
            if let Some(plan) = &entry.plan {
                if !(1..=20).contains(&plan.label.chars().count())
                    || !plan.label.chars().all(|c| !c.is_control())
                {
                    bail!("plan label must have 1–20 printable characters")
                }
                valid_amount(plan.fee_usd)?;
            }
            if let Some(rate) = &entry.rate {
                if !rate.model_prefixes.iter().all(|p| valid_prefix(p)) {
                    bail!("model prefix must have 1–64 printable ASCII characters")
                }
                match &rate.price {
                    RatePrice::Flat { flat } => {
                        valid_amount(*flat)?;
                    }
                    RatePrice::PerType(p) => {
                        for x in [
                            p.input,
                            p.output,
                            p.cache_write_5m,
                            p.cache_write_1h,
                            p.cache_read,
                        ] {
                            valid_amount(x)?;
                        }
                    }
                }
            }
            for topup in &entry.top_ups {
                valid_amount(topup.usd)?;
                if topup.date > today {
                    bail!("top-up date is in the future")
                }
            }
        }
        Ok(())
    }
}
pub fn read(dir: &Path, today: NaiveDate) -> Result<Billing> {
    let path = dir.join("billing.json");
    if !path.exists() {
        return Ok(Billing {
            version: 1,
            ..Billing::default()
        });
    }
    let raw: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
    if let Some(profiles) = raw.get("profiles").and_then(|v| v.as_object()) {
        for entry in profiles.values() {
            if let Some(rate) = entry.get("rate").and_then(|v| v.as_object()) {
                let allowed = [
                    "model_prefixes",
                    "flat",
                    "input",
                    "output",
                    "cache_write_5m",
                    "cache_write_1h",
                    "cache_read",
                ];
                if rate.keys().any(|key| !allowed.contains(&key.as_str())) {
                    bail!("unknown billing rate field")
                }
                if rate.contains_key("flat")
                    && rate.keys().any(|key| allowed[2..].contains(&key.as_str()))
                {
                    bail!("flat conflicts with per-type rates")
                }
            }
        }
    }
    let billing: Billing = serde_json::from_value(raw)?;
    billing.validate(today)?;
    Ok(billing)
}
pub fn edit<F>(store: &Store, today: NaiveDate, change: F) -> Result<()>
where
    F: FnOnce(&mut Billing) -> Result<()>,
{
    let Some(_lock) = store.try_lock()? else {
        bail!("usage store busy; try again")
    };
    let mut billing = read(&store.dir, today)?;
    change(&mut billing)?;
    billing.validate(today)?;
    atomic::write_private(
        &store.dir.join("billing.json"),
        &serde_json::to_vec_pretty(&billing)?,
    )
}
pub fn purge(store: &Store, profile: &str) -> Result<()> {
    let today = Local::now().date_naive();
    edit(store, today, |billing| {
        billing.profiles.remove(profile);
        Ok(())
    })
}

pub fn price_tokens(
    input: u64,
    output: u64,
    write5: u64,
    write1: u64,
    read: u64,
    price: &Price,
) -> Cost {
    let amount = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    Cost {
        input: amount(input, price.input),
        output: amount(output, price.output),
        cache_write_5m: amount(write5, price.cache_write_5m),
        cache_write_1h: amount(write1, price.cache_write_1h),
        cache_read: amount(read, price.cache_read),
    }
}
pub fn price(
    model: &str,
    speed: Option<&str>,
    tokens: [u64; 5],
    rates: &Rates,
    entry: Option<&ProfileBilling>,
    per_token: bool,
) -> (Option<f64>, Option<f64>) {
    let list = rates
        .model_rate(model)
        .and_then(|m| {
            if speed == Some("fast") {
                m.fast.as_ref()
            } else {
                Some(&m.standard)
            }
        })
        .map(|p| price_tokens(tokens[0], tokens[1], tokens[2], tokens[3], tokens[4], p).total());
    let spend = if per_token {
        entry
            .and_then(|e| e.rate.as_ref())
            .filter(|r| {
                r.model_prefixes.is_empty() || r.model_prefixes.iter().any(|p| model.starts_with(p))
            })
            .map(|r| match &r.price {
                RatePrice::Flat { flat } => tokens.iter().sum::<u64>() as f64 * flat / 1_000_000.0,
                RatePrice::PerType(p) => {
                    price_tokens(tokens[0], tokens[1], tokens[2], tokens[3], tokens[4], p).total()
                }
            })
            .or(list)
    } else {
        None
    };
    (list, spend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::rates;
    #[test]
    fn flat_rate_prices_all_five_kinds_without_list_multipliers() {
        // Known-bad: applying list-price cache multipliers to a flat gateway rate.
        let entry = ProfileBilling {
            rate: Some(Rate {
                model_prefixes: vec!["acme/".into()],
                price: RatePrice::Flat { flat: 1.0 },
            }),
            ..Default::default()
        };
        let (list, spend) = price(
            "acme/claude-x.5",
            None,
            [1_000_000; 5],
            &rates::seed(),
            Some(&entry),
            true,
        );
        assert!(list.is_none());
        assert_eq!(spend, Some(5.0));
    }
    #[test]
    fn prefix_scoping_preserves_list_price_for_direct_models() {
        // Known-bad: charging the flat gateway rate on an unprefixed model.
        let entry = ProfileBilling {
            rate: Some(Rate {
                model_prefixes: vec!["acme/".into()],
                price: RatePrice::Flat { flat: 1.0 },
            }),
            ..Default::default()
        };
        let rates = rates::seed();
        let (list, spend) = price(
            "claude-opus-5",
            None,
            [1_000_000, 0, 0, 0, 0],
            &rates,
            Some(&entry),
            true,
        );
        assert_eq!(list, Some(5.0));
        assert_eq!(spend, list);
        assert_eq!(
            price(
                "acme/haiku",
                None,
                [1_000_000, 0, 0, 0, 0],
                &rates,
                Some(&entry),
                true
            )
            .1,
            Some(1.0)
        );
    }
    #[test]
    fn plan_has_no_per_request_spend_and_unknown_fields_are_rejected() {
        // Known-bad: summing list value into a subscription bill or dropping unknown settings.
        let rates = rates::seed();
        let (list, spend) = price(
            "claude-opus-5",
            None,
            [1_000_000, 0, 0, 0, 0],
            &rates,
            None,
            false,
        );
        assert_eq!(list, Some(5.0));
        assert_eq!(spend, None);
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("billing.json"),
            r#"{"version":1,"profiles":{"p":{"rate":{"flat":1,"unknown":2}}}}"#,
        )
        .unwrap();
        assert!(read(tmp.path(), NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()).is_err());
    }
    #[test]
    fn flat_and_per_type_rates_roundtrip_with_strict_fields() {
        // Known-bad: flattening an untagged rate writes JSON that the next edit cannot read.
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().to_path_buf(), vec![]);
        let today = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        edit(&store, today, |b| {
            b.profiles.insert(
                "flat".into(),
                ProfileBilling {
                    rate: Some(Rate {
                        model_prefixes: vec!["acme/".into()],
                        price: RatePrice::Flat { flat: 1.0 },
                    }),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        assert!(
            read(tmp.path(), today).unwrap().profiles["flat"]
                .rate
                .is_some()
        );
        edit(&store, today, |b| {
            b.profiles.insert(
                "typed".into(),
                ProfileBilling {
                    rate: Some(Rate {
                        model_prefixes: vec![],
                        price: RatePrice::PerType(Price {
                            input: 1.0,
                            output: 2.0,
                            cache_write_5m: 3.0,
                            cache_write_1h: 4.0,
                            cache_read: 5.0,
                        }),
                    }),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(read(tmp.path(), today).unwrap().profiles.len(), 2);
    }
    #[test]
    fn private_write_preserves_other_profiles_and_busy_lock_refuses() {
        // Known-bad: a world-readable file, dropping another entry, or waiting on a busy lock.
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path().to_path_buf(), vec![]);
        let today = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        edit(&store, today, |b| {
            b.profiles.insert("a".into(), ProfileBilling::default());
            Ok(())
        })
        .unwrap();
        edit(&store, today, |b| {
            b.profiles.insert("b".into(), ProfileBilling::default());
            Ok(())
        })
        .unwrap();
        assert_eq!(read(tmp.path(), today).unwrap().profiles.len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(tmp.path().join("billing.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let lock = store.try_lock().unwrap().unwrap();
        assert!(
            edit(&store, today, |_| Ok(()))
                .unwrap_err()
                .to_string()
                .contains("busy")
        );
        drop(lock);
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test]
    fn invalid_amounts_and_future_topups_are_refused() {
        // Known-bad: accepting NaN, negative dollars, or future-dated credit.
        assert!(valid_amount(f64::NAN).is_err());
        assert!(valid_amount(-1.0).is_err());
        let date = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
        let entry = ProfileBilling {
            top_ups: vec![TopUp {
                date: date.succ_opt().unwrap(),
                usd: 1.0,
            }],
            ..Default::default()
        };
        assert!(
            Billing {
                version: 1,
                profiles: BTreeMap::from([("p".into(), entry)])
            }
            .validate(date)
            .is_err()
        );
    }
}
