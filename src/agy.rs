//! Antigravity profile HOME management. All inputs are explicit paths or bytes.

use crate::atomic;
use crate::codex::jwt_claims;
use crate::profile::{copy_dir_all_filtered, copy_symlink};
use anyhow::{Result, bail};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const AGY_PROGRAM: &str = "agy";
pub const AGY_AUTH_CHECK_ARGS: &[&str] = &["models"];
pub const AGY_TOKEN_RELATIVE: &str = ".gemini/antigravity-cli/antigravity-oauth-token";
pub const AGY_SEED_ALLOWLIST: &[&str] = &[
    "config/mcp_config.json",
    "antigravity-cli/settings.json",
    "antigravity-cli/mcp_config.json",
    "antigravity-cli/skills",
    "skills",
];
pub const AGY_ACTIVITY_MARKERS: &[&str] = &[
    "home/.gemini/antigravity-cli/log",
    "home/.gemini/antigravity-cli/conversations",
    "home/.gemini/antigravity-cli/brain",
];

const FARM_MANIFEST: &str = "agy-links.json";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkFarmReport {
    pub unreadable_record: bool,
}

pub fn link_farm_warning(report: &LinkFarmReport) -> Option<&'static str> {
    report.unreadable_record.then_some(
        "could not read agy-links.json; old dangling links were kept and the record was rebuilt",
    )
}

pub fn ensure_supported(unix: bool) -> Result<()> {
    if !unix {
        bail!("Antigravity profiles are supported on Unix only.");
    }
    Ok(())
}

pub fn profile_home(profile_dir: &Path) -> PathBuf {
    profile_dir.join("home")
}

/// Clean only the entries a login staged in an already-existing empty profile.
pub fn cleanup_staged_home(profile_dir: &Path) {
    let home = profile_home(profile_dir);
    match fs::symlink_metadata(&home) {
        Ok(meta) if meta.is_dir() => {
            let _ = fs::remove_dir_all(&home);
        }
        Ok(_) => {
            let _ = fs::remove_file(&home);
        }
        Err(_) => {}
    }
    let _ = fs::remove_file(profile_dir.join(FARM_MANIFEST));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Missing,
    Empty,
    NonEmpty,
}

fn real_parent_chain(root: &Path, relative: &Path) -> bool {
    if !fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir()) {
        return false;
    }
    let mut parent = relative.parent();
    while let Some(path) = parent {
        if path.as_os_str().is_empty() {
            break;
        }
        if !fs::symlink_metadata(root.join(path)).is_ok_and(|meta| meta.is_dir()) {
            return false;
        }
        parent = path.parent();
    }
    true
}

pub fn token_state(home: &Path) -> TokenState {
    if !real_parent_chain(home, Path::new(AGY_TOKEN_RELATIVE)) {
        return TokenState::Missing;
    }
    match fs::symlink_metadata(home.join(AGY_TOKEN_RELATIVE)) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => TokenState::NonEmpty,
        Ok(meta) if meta.is_file() => TokenState::Empty,
        _ => TokenState::Missing,
    }
}

pub fn identity_from_token(bytes: &[u8]) -> Option<String> {
    let token: Value = serde_json::from_slice(bytes).ok()?;
    let claims = jwt_claims(token.get("id_token")?.as_str()?)?;
    claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_string)
}

pub fn seed_gemini(real_home: &Path, profile_home: &Path) -> Result<()> {
    if !fs::symlink_metadata(profile_home).is_ok_and(|meta| meta.is_dir()) {
        bail!("Antigravity profile HOME is not a directory.");
    }
    let source = real_home.join(".gemini");
    let destination = profile_home.join(".gemini");
    match fs::symlink_metadata(&destination) {
        Ok(meta) if !meta.is_dir() => bail!("Antigravity profile .gemini is not a directory."),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&destination)?,
        Err(error) => return Err(error.into()),
    }
    if !fs::symlink_metadata(&source).is_ok_and(|meta| meta.is_dir()) {
        return Ok(());
    }
    for relative in AGY_SEED_ALLOWLIST {
        let from = source.join(relative);
        // Refuse a linked parent: a seed must never walk out of the source tree.
        if !real_parent_chain(&source, Path::new(relative)) {
            continue;
        }
        let meta = match fs::symlink_metadata(&from) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let to = destination.join(relative);
        fs::create_dir_all(to.parent().expect("seed entry has a parent"))?;
        if meta.file_type().is_symlink() {
            copy_symlink(&from, &to)?;
        } else if meta.is_dir() {
            copy_dir_all_filtered(&from, &to, &[])?;
        } else if meta.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct FarmHealth {
    pub links: usize,
    pub dangling: usize,
    pub local: Vec<String>,
}

pub fn farm_health(home: &Path) -> Result<FarmHealth> {
    if !fs::symlink_metadata(home).is_ok_and(|meta| meta.is_dir()) {
        bail!("Antigravity profile HOME is not a directory.");
    }
    let mut health = FarmHealth::default();
    for entry in fs::read_dir(home)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            health.links += 1;
            if fs::metadata(&path).is_err() {
                health.dangling += 1;
            }
        } else if entry.file_name() != ".gemini" {
            health
                .local
                .push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    health.local.sort();
    Ok(health)
}

pub fn local_summary(entries: &[String], limit: usize) -> String {
    let mut names: Vec<String> = entries
        .iter()
        .map(|name| name.replace(['\r', '\n'], " "))
        .collect();
    names.sort();
    let shown = names
        .iter()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > limit {
        format!("{shown}, and {} more", names.len() - limit)
    } else {
        shown
    }
}

pub fn activity_root_is_local(profile_dir: &Path) -> bool {
    AGY_ACTIVITY_MARKERS
        .iter()
        .all(|marker| real_parent_chain(profile_dir, Path::new(marker)))
}

#[cfg(unix)]
pub fn link_farm(real_home: &Path, profile_dir: &Path) -> Result<LinkFarmReport> {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;

    if !fs::symlink_metadata(profile_dir).is_ok_and(|meta| meta.is_dir()) {
        bail!("Antigravity profile directory is not a directory.");
    }
    let home = profile_home(profile_dir);
    match fs::symlink_metadata(&home) {
        Ok(meta) if !meta.is_dir() => bail!("Antigravity profile HOME is not a directory."),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&home)?,
        Err(error) => return Err(error.into()),
    }
    let gemini = home.join(".gemini");
    match fs::symlink_metadata(&gemini) {
        Ok(meta) if !meta.is_dir() => bail!("Antigravity profile .gemini is not a directory."),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&gemini)?,
        Err(error) => return Err(error.into()),
    }
    let manifest = profile_dir.join(FARM_MANIFEST);
    let (mut created, report): (Vec<Vec<u8>>, LinkFarmReport) = match fs::read(&manifest) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(names) => (names, LinkFarmReport::default()),
            Err(_) => (
                Vec::new(),
                LinkFarmReport {
                    unreadable_record: true,
                },
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (Vec::new(), LinkFarmReport::default())
        }
        Err(_) => (
            Vec::new(),
            LinkFarmReport {
                unreadable_record: true,
            },
        ),
    };
    let mut retained = Vec::new();
    for raw_name in created.drain(..) {
        let name = OsString::from_vec(raw_name.clone());
        let source = real_home.join(&name);
        let destination = home.join(&name);
        let source_present = match fs::symlink_metadata(&source) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        let owned_link = match fs::symlink_metadata(&destination) {
            Ok(meta) if meta.file_type().is_symlink() => fs::read_link(&destination)? == source,
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !source_present && owned_link {
            // This is a link recorded by cswitch, with its original target.
            fs::remove_file(&destination)?;
        }
        if source_present && owned_link {
            retained.push(raw_name);
        }
    }
    created = retained;
    for entry in fs::read_dir(real_home)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".gemini" || name == ".claude-switch" {
            continue;
        }
        let destination = home.join(&name);
        match fs::symlink_metadata(&destination) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        symlink(entry.path(), &destination)?;
        created.push(name.as_bytes().to_vec());
    }
    atomic::write(&manifest, &serde_json::to_vec(&created)?)?;
    Ok(report)
}

#[cfg(not(unix))]
pub fn link_farm(_real_home: &Path, _profile_dir: &Path) -> Result<LinkFarmReport> {
    unsupported_link_farm()
}

#[cfg_attr(unix, allow(dead_code))]
fn unsupported_link_farm() -> Result<LinkFarmReport> {
    bail!("Antigravity profiles are supported on Unix only.")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::profile::{Profile, ProfileManager, Registry, Tool};
    use chrono::Utc;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let real = temp.path().join("real");
        let profile = real.join(".claude-switch/profiles/test");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(real.join(".gemini")).unwrap();
        fs::write(real.join("note"), b"kept bytes").unwrap();
        fs::create_dir(real.join("documents")).unwrap();
        fs::write(real.join("documents/entry"), b"nested bytes").unwrap();
        (temp, real, profile)
    }

    #[test]
    fn farm_links_every_real_entry_except_gemini_and_switch() {
        // Known-bad: linking .claude-switch creates a cycle into the profile store.
        let (_temp, real, profile) = fixture();
        link_farm(&real, &profile).unwrap();
        let home = profile_home(&profile);
        assert!(fs::symlink_metadata(home.join(".gemini")).unwrap().is_dir());
        assert!(fs::symlink_metadata(home.join(".claude-switch")).is_err());
        for name in ["note", "documents"] {
            assert!(
                fs::symlink_metadata(home.join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_link(home.join(name)).unwrap(), real.join(name));
        }
        assert_eq!(farm_health(&home).unwrap().links, 2);
    }

    #[test]
    fn relink_adds_new_and_only_removes_owned_dangling_links() {
        // Known-bad: deleting every destination that lacks a real-HOME counterpart removes a program's local file.
        let (_temp, real, profile) = fixture();
        link_farm(&real, &profile).unwrap();
        let home = profile_home(&profile);
        fs::write(home.join("local"), b"local bytes").unwrap();
        symlink(real.join("absent"), home.join("foreign")).unwrap();
        fs::remove_file(real.join("note")).unwrap();
        fs::write(real.join("new"), b"new bytes").unwrap();
        link_farm(&real, &profile).unwrap();
        assert!(fs::symlink_metadata(home.join("note")).is_err());
        assert_eq!(fs::read(home.join("local")).unwrap(), b"local bytes");
        assert!(fs::symlink_metadata(home.join("foreign")).is_ok());
        assert_eq!(fs::read_link(home.join("new")).unwrap(), real.join("new"));
        assert_eq!(farm_health(&home).unwrap().dangling, 1);
    }

    #[test]
    fn recorded_link_replaced_by_file_stays_with_and_without_source() {
        // Known-bad: a recorded name overrides a local copy, or removes it when source disappears.
        for remove_source in [false, true] {
            let (_temp, real, profile) = fixture();
            link_farm(&real, &profile).unwrap();
            let local = profile_home(&profile).join("note");
            fs::remove_file(&local).unwrap();
            fs::write(&local, b"profile-owned copy").unwrap();
            if remove_source {
                fs::remove_file(real.join("note")).unwrap();
            }
            link_farm(&real, &profile).unwrap();
            assert_eq!(fs::read(&local).unwrap(), b"profile-owned copy");
            assert!(
                !fs::symlink_metadata(&local)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }

    #[test]
    fn recorded_link_repointed_elsewhere_is_not_removed() {
        // Known-bad: any link at a recorded name counts as owned.
        let (_temp, real, profile) = fixture();
        link_farm(&real, &profile).unwrap();
        let local = profile_home(&profile).join("note");
        fs::remove_file(&local).unwrap();
        let other = real.join("documents");
        symlink(&other, &local).unwrap();
        fs::remove_file(real.join("note")).unwrap();
        link_farm(&real, &profile).unwrap();
        assert_eq!(fs::read_link(&local).unwrap(), other);
    }

    #[test]
    fn token_state_refuses_links_and_distinguishes_empty_tokens() {
        // Known-bad: metadata follows a linked token or antigravity-cli parent.
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let cli = home.join(".gemini/antigravity-cli");
        fs::create_dir_all(&cli).unwrap();
        let token = cli.join("antigravity-oauth-token");
        assert_eq!(token_state(&home), TokenState::Missing);
        fs::write(&token, b"").unwrap();
        assert_eq!(token_state(&home), TokenState::Empty);
        fs::write(&token, b"synthetic").unwrap();
        assert_eq!(token_state(&home), TokenState::NonEmpty);
        fs::remove_file(&token).unwrap();
        let outside = temp.path().join("outside-token");
        fs::write(&outside, b"synthetic").unwrap();
        symlink(&outside, &token).unwrap();
        assert_eq!(token_state(&home), TokenState::Missing);
        fs::remove_file(&token).unwrap();
        fs::remove_dir(&cli).unwrap();
        let outside_cli = temp.path().join("outside-cli");
        fs::create_dir(&outside_cli).unwrap();
        fs::write(outside_cli.join("antigravity-oauth-token"), b"synthetic").unwrap();
        symlink(&outside_cli, &cli).unwrap();
        assert_eq!(token_state(&home), TokenState::Missing);
    }

    #[test]
    fn farm_refuses_linked_home_and_gemini_without_touching_targets() {
        // Known-bad: is_dir follows either linked destination directory.
        for linked_home in [true, false] {
            let (temp, real, profile) = fixture();
            let target = temp.path().join("outside");
            fs::create_dir(&target).unwrap();
            let home = profile_home(&profile);
            if linked_home {
                symlink(&target, &home).unwrap();
            } else {
                fs::create_dir(&home).unwrap();
                symlink(&target, home.join(".gemini")).unwrap();
            }
            assert!(link_farm(&real, &profile).is_err());
            assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
            assert!(!profile.join(FARM_MANIFEST).exists());
        }
    }

    #[test]
    fn linked_gemini_seed_source_copies_nothing() {
        // Known-bad: metadata follows the real HOME's linked .gemini source.
        let (temp, real, profile) = fixture();
        let source = real.join(".gemini");
        fs::remove_dir(&source).unwrap();
        let outside = temp.path().join("outside-gemini");
        fs::create_dir_all(outside.join("skills/warm")).unwrap();
        fs::write(outside.join("skills/warm/entry"), b"must stay outside").unwrap();
        symlink(&outside, &source).unwrap();
        let home = profile_home(&profile);
        fs::create_dir(&home).unwrap();
        seed_gemini(&real, &home).unwrap();
        assert_eq!(fs::read_dir(home.join(".gemini")).unwrap().count(), 0);
        assert_eq!(
            fs::read(outside.join("skills/warm/entry")).unwrap(),
            b"must stay outside"
        );
    }

    #[test]
    fn blank_email_claim_is_unavailable() {
        // Known-bad: dropping the empty-email filter reports an empty address.
        assert_eq!(
            identity_from_token(br#"{"id_token":"h.eyJlbWFpbCI6IiJ9.s"}"#),
            None
        );
    }

    #[test]
    fn link_ownership_survives_an_unchanged_second_run() {
        // Known-bad: retained entries omitted from the second link record.
        let (_temp, real, profile) = fixture();
        link_farm(&real, &profile).unwrap();
        link_farm(&real, &profile).unwrap();
        fs::remove_file(real.join("note")).unwrap();
        link_farm(&real, &profile).unwrap();
        assert!(fs::symlink_metadata(profile_home(&profile).join("note")).is_err());
    }

    #[test]
    fn damaged_link_record_is_rebuilt_without_deleting_old_links() {
        // Known-bad: serde_json::from_slice(&bytes)? blocks every launch on a damaged record.
        for damaged in [b"not json".as_slice(), b"{}".as_slice()] {
            let (_temp, real, profile) = fixture();
            assert_eq!(
                link_farm(&real, &profile).unwrap(),
                LinkFarmReport::default()
            );
            let home = profile_home(&profile);
            fs::remove_file(real.join("note")).unwrap();
            fs::write(home.join("local"), b"local bytes").unwrap();
            fs::write(real.join("new"), b"new bytes").unwrap();
            fs::write(profile.join(FARM_MANIFEST), damaged).unwrap();

            let report = link_farm(&real, &profile).unwrap();
            assert!(report.unreadable_record);
            assert!(link_farm_warning(&report).unwrap().contains(FARM_MANIFEST));
            assert_eq!(fs::read_link(home.join("note")).unwrap(), real.join("note"));
            assert_eq!(fs::read(home.join("local")).unwrap(), b"local bytes");
            assert_eq!(fs::read_link(home.join("new")).unwrap(), real.join("new"));
            let names: Vec<Vec<u8>> =
                serde_json::from_slice(&fs::read(profile.join(FARM_MANIFEST)).unwrap()).unwrap();
            assert!(!names.is_empty());
        }
    }

    #[test]
    fn remove_profile_never_removes_real_home_entries() {
        // Known-bad: a recursive remover follows a farm link and deletes real-HOME content.
        let (_temp, real, profile) = fixture();
        link_farm(&real, &profile).unwrap();
        let manager = ProfileManager::with_base_dir(real.join(".claude-switch")).unwrap();
        let mut registry = Registry::default();
        registry.profiles.insert(
            "test".into(),
            Profile {
                name: "test".into(),
                tool: Tool::Antigravity,
                email: Some("test@example.com".into()),
                added: Utc::now(),
                last_used: None,
            },
        );
        fs::write(
            real.join(".claude-switch/registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        manager.remove_profile("test").unwrap();
        assert!(!profile.exists());
        assert_eq!(fs::read(real.join("note")).unwrap(), b"kept bytes");
        assert_eq!(
            fs::read(real.join("documents/entry")).unwrap(),
            b"nested bytes"
        );
        assert!(fs::symlink_metadata(real.join(".gemini")).unwrap().is_dir());
        assert!(manager.list_profiles().unwrap().is_empty());
    }

    #[test]
    fn seed_copies_only_five_warm_paths() {
        // Known-bad: copying antigravity-oauth-token imports the source account.
        let (_temp, real, profile) = fixture();
        let source = real.join(".gemini");
        for name in [
            "antigravity",
            "antigravity-ide",
            "history",
            "tmp",
            "skills",
            "config",
            "antigravity-cli",
        ] {
            fs::create_dir_all(source.join(name)).unwrap();
        }
        for name in [
            "google_accounts.json",
            "installation_id",
            "oauth_creds.json",
            "projects.json",
        ] {
            fs::write(source.join(name), b"excluded synthetic state").unwrap();
        }
        fs::write(source.join("config/mcp_config.json"), b"warm config").unwrap();
        fs::write(source.join("config/.migrated"), b"excluded").unwrap();
        fs::create_dir_all(source.join("config/projects")).unwrap();
        fs::write(source.join("skills/warm"), b"warm skill").unwrap();
        fs::create_dir_all(source.join("antigravity-cli/skills")).unwrap();
        for name in ["settings.json", "mcp_config.json"] {
            fs::write(source.join("antigravity-cli").join(name), b"warm config").unwrap();
        }
        fs::write(source.join("antigravity-cli/skills/warm"), b"warm skill").unwrap();
        for name in [
            "annotations",
            "bin",
            "brain",
            "builtin",
            "cache",
            "conversations",
            "crashes",
            "implicit",
            "knowledge",
            "log",
            "mcp",
            "presence",
            "scratch",
            "updater",
        ] {
            fs::create_dir(source.join("antigravity-cli").join(name)).unwrap();
            fs::write(
                source.join("antigravity-cli").join(name).join("entry"),
                b"excluded",
            )
            .unwrap();
        }
        for name in [
            "antigravity-oauth-token",
            "cli.log",
            "conversation_summaries.db",
            "history.jsonl",
            "installation_id",
            "jetbox_summaries_proto.pb",
            "jetski_state.pbtxt",
            "last_check.timestamp",
        ] {
            fs::write(
                source.join("antigravity-cli").join(name),
                b"excluded synthetic state",
            )
            .unwrap();
        }
        link_farm(&real, &profile).unwrap();
        let home = profile_home(&profile);
        seed_gemini(&real, &home).unwrap();
        let seeded = home.join(".gemini");
        let mut top: Vec<_> = fs::read_dir(&seeded)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        top.sort();
        assert_eq!(top, ["antigravity-cli", "config", "skills"]);
        let mut cli: Vec<_> = fs::read_dir(seeded.join("antigravity-cli"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        cli.sort();
        assert_eq!(cli, ["mcp_config.json", "settings.json", "skills"]);
        assert_eq!(
            fs::read(seeded.join("config/mcp_config.json")).unwrap(),
            b"warm config"
        );
        assert_eq!(fs::read(seeded.join("skills/warm")).unwrap(), b"warm skill");
        assert!(fs::symlink_metadata(home.join(AGY_TOKEN_RELATIVE)).is_err());
        assert!(fs::symlink_metadata(seeded.join("oauth_creds.json")).is_err());
        assert!(fs::symlink_metadata(seeded.join("google_accounts.json")).is_err());
    }

    #[test]
    fn seed_preserves_leaf_links_without_walking_linked_parents() {
        // Known-bad: a copied skills link is dereferenced, or a linked config parent is walked.
        let (temp, real, profile) = fixture();
        let source = real.join(".gemini");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("mcp_config.json"), b"outside config").unwrap();
        fs::write(outside.join("skill"), b"outside skill").unwrap();
        symlink(&outside, source.join("config")).unwrap();
        symlink(&outside, source.join("skills")).unwrap();
        link_farm(&real, &profile).unwrap();
        let home = profile_home(&profile);
        seed_gemini(&real, &home).unwrap();
        assert!(
            fs::symlink_metadata(home.join(".gemini/skills"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(home.join(".gemini/config/mcp_config.json")).is_err());
        assert_eq!(
            fs::read(outside.join("mcp_config.json")).unwrap(),
            b"outside config"
        );
    }

    #[test]
    fn token_identity_reads_only_own_jwt_email() {
        // Known-bad: reading a Gemini CLI leftover or echoing a malformed token as identity.
        let token = br#"{"id_token":"h.eyJlbWFpbCI6Im9AZXhhbXBsZS5jb20ifQ.s","token":{"access_token":"synthetic"}}"#;
        assert_eq!(identity_from_token(token), Some("o@example.com".into()));
        assert_eq!(identity_from_token(br#"{"id_token":"bad"}"#), None);
        assert_eq!(identity_from_token(br#"{"id_token":"h.e30.s"}"#), None);
    }

    #[test]
    fn non_unix_is_a_single_line_refusal() {
        // Known-bad: trying to create an Antigravity farm with Windows copy fallback.
        assert_eq!(
            ensure_supported(false).unwrap_err().to_string(),
            "Antigravity profiles are supported on Unix only."
        );
    }

    #[test]
    fn non_unix_link_farm_helper_has_report_result_and_refuses() {
        // Known-bad: the non-Unix farm body returns Result<()> and fails to compile.
        assert_eq!(
            unsupported_link_farm().unwrap_err().to_string(),
            "Antigravity profiles are supported on Unix only."
        );
    }
}
