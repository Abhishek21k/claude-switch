pub mod attribute;
pub mod billing;
pub mod ledger;
pub mod metrics;
pub mod parse;
pub mod rates;
pub mod report;

use crate::profile::{ProfileManager, Tool};
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
    for profile in manager
        .list_profiles()?
        .into_iter()
        .filter(|p| p.tool == Tool::Claude)
    {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Profile, Registry};
    use chrono::Utc;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn codex_home_is_never_a_usage_source() {
        // Known-bad: the ledger walks a Codex home as if its sessions were Claude JSONL.
        let temp = TempDir::new().unwrap();
        let manager =
            ProfileManager::with_paths(temp.path().join("switch"), temp.path().join(".claude"))
                .unwrap();
        let mut registry = Registry::default();
        for (name, tool) in [("c", Tool::Claude), ("o", Tool::Codex)] {
            registry.profiles.insert(
                name.into(),
                Profile {
                    name: name.into(),
                    tool,
                    email: Some(format!("{name}@example.com")),
                    added: Utc::now(),
                    last_used: None,
                },
            );
            fs::create_dir_all(manager.profile_dir(name)).unwrap();
        }
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        let sources = store(&manager, None).unwrap().sources;
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|source| source.profile == "c"));
        assert!(!sources.iter().any(|source| source.profile == "o"));
    }
}
