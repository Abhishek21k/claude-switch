pub mod ledger;
pub mod parse;

use crate::profile::ProfileManager;
use anyhow::Result;
use ledger::{Source, Store};
use std::collections::HashSet;
use std::path::PathBuf;

pub fn store(manager: &ProfileManager, override_dir: Option<PathBuf>) -> Result<Store> {
    let home = manager
        .base_dir
        .parent()
        .expect("profile base has a parent");
    let mut sources = vec![Source {
        profile: "default".to_string(),
        directory: home.join(".claude"),
    }];
    for profile in manager.list_profiles()? {
        sources.push(Source {
            directory: manager.profile_dir(&profile.name),
            profile: profile.name,
        });
    }
    Ok(Store::new(
        override_dir.unwrap_or_else(|| manager.base_dir.join("usage")),
        sources,
    ))
}

pub fn plain_report(store: &Store) -> Result<String> {
    let ingest = store.ingest()?;
    let ledger = store.load()?;
    let sessions = ledger
        .requests
        .iter()
        .map(|row| (&row.profile, &row.session))
        .collect::<HashSet<_>>();
    let input: u64 = ledger.requests.iter().map(|row| row.input).sum();
    let output: u64 = ledger.requests.iter().map(|row| row.output).sum();
    let cache_write: u64 = ledger
        .requests
        .iter()
        .map(|row| row.cache_write_5m + row.cache_write_1h)
        .sum();
    let cache_read: u64 = ledger.requests.iter().map(|row| row.cache_read).sum();
    let earliest = ledger
        .requests
        .iter()
        .map(|row| row.time)
        .min()
        .map(|time| time.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "none".to_string());
    Ok(format!(
        "Requests: {}  Sessions: {}\nInput: {input}  Output: {output}  Cache write: {cache_write}  Cache read: {cache_read}\nLedger: {} requests; {} duplicates collapsed; {} unreadable lines; earliest {earliest}{}\n",
        ledger.requests.len(),
        sessions.len(),
        ledger.requests.len(),
        ledger.cursors.duplicates,
        ledger.cursors.malformed + ingest.partial,
        if ingest.skipped_lock {
            " (ingest skipped: locked)"
        } else {
            ""
        },
    ))
}
