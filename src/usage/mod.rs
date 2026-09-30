pub mod attribute;
pub mod ledger;
pub mod parse;
pub mod rates;
pub mod report;

use crate::profile::ProfileManager;
use anyhow::Result;
use ledger::{Source, Store};
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
