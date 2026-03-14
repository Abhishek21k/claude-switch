use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use crate::profile::{Profile, ProfileManager, Registry};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build a ProfileManager that stores everything inside a temp dir.
fn make_manager(tmp: &TempDir) -> ProfileManager {
    let base_dir = tmp.path().join(".claude-switch");
    let profiles_dir = base_dir.join("profiles");
    let registry_path = base_dir.join("registry.json");
    fs::create_dir_all(&profiles_dir).unwrap();
    ProfileManager {
        base_dir,
        profiles_dir,
        registry_path,
    }
}

/// Write a minimal ~/.claude directory structure into `dir`.
fn write_fake_claude_dir(dir: &PathBuf, email: &str) {
    fs::create_dir_all(dir).unwrap();

    // .claude.json  (email lives under oauthAccount)
    let claude_json = serde_json::json!({
        "oauthAccount": {
            "emailAddress": email,
            "accountUuid": "test-uuid-1234"
        }
    });
    fs::write(
        dir.join(".claude.json"),
        serde_json::to_string_pretty(&claude_json).unwrap(),
    )
    .unwrap();

    // .credentials.json
    let creds_json = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "tok_test",
            "refreshToken": "ref_test",
            "expiresAt": 9_999_999_999_u64
        }
    });
    fs::write(
        dir.join(".credentials.json"),
        serde_json::to_string_pretty(&creds_json).unwrap(),
    )
    .unwrap();
}

// ── registry ─────────────────────────────────────────────────────────────────

#[test]
fn test_empty_registry_on_fresh_manager() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);
    let reg = mgr.load_registry().unwrap();
    assert!(reg.profiles.is_empty());
}

#[test]
fn test_registry_round_trips() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let mut reg = Registry::default();
    reg.profiles.insert(
        "work".into(),
        Profile {
            name: "work".into(),
            email: Some("work@example.com".into()),
            added: chrono::Utc::now(),
            last_used: None,
        },
    );

    mgr.save_registry_pub(&reg).unwrap();
    let loaded = mgr.load_registry().unwrap();
    assert_eq!(loaded.profiles.len(), 1);
    assert_eq!(
        loaded.profiles["work"].email.as_deref(),
        Some("work@example.com")
    );
}

// ── add_profile ───────────────────────────────────────────────────────────────

#[test]
fn test_add_profile_copies_files_and_records_email() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let fake_claude = tmp.path().join(".claude");
    write_fake_claude_dir(&fake_claude, "user@test.com");

    let profile = mgr
        .add_profile_from("work", &fake_claude)
        .unwrap();

    assert_eq!(profile.name, "work");
    assert_eq!(profile.email.as_deref(), Some("user@test.com"));

    // Destination directory must exist with the copied files
    let dest = mgr.profile_dir("work");
    assert!(dest.join(".claude.json").exists());
    assert!(dest.join(".credentials.json").exists());
}

#[test]
fn test_add_profile_registers_in_registry() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let fake_claude = tmp.path().join(".claude");
    write_fake_claude_dir(&fake_claude, "a@b.com");

    mgr.add_profile_from("personal", &fake_claude).unwrap();

    let reg = mgr.load_registry().unwrap();
    assert!(reg.profiles.contains_key("personal"));
}

#[test]
fn test_add_duplicate_profile_errors_without_force() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let fake_claude = tmp.path().join(".claude");
    write_fake_claude_dir(&fake_claude, "a@b.com");

    mgr.add_profile_from("dup", &fake_claude).unwrap();
    let err = mgr.add_profile_from("dup", &fake_claude).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn test_add_profile_force_overwrites() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let fake_claude = tmp.path().join(".claude");
    write_fake_claude_dir(&fake_claude, "first@test.com");
    mgr.add_profile_from("slot", &fake_claude).unwrap();

    // Overwrite with different email
    write_fake_claude_dir(&fake_claude, "second@test.com");
    mgr.add_profile_from_force("slot", &fake_claude).unwrap();

    let reg = mgr.load_registry().unwrap();
    assert_eq!(
        reg.profiles["slot"].email.as_deref(),
        Some("second@test.com")
    );
}

// ── list_profiles ─────────────────────────────────────────────────────────────

#[test]
fn test_list_profiles_returns_sorted() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    for name in &["zebra", "alpha", "mango"] {
        let fake_claude = tmp.path().join(format!(".claude-{name}"));
        write_fake_claude_dir(&fake_claude, &format!("{name}@test.com"));
        mgr.add_profile_from(name, &fake_claude).unwrap();
    }

    let profiles = mgr.list_profiles().unwrap();
    let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "mango", "zebra"]);
}

// ── remove_profile ────────────────────────────────────────────────────────────

#[test]
fn test_remove_profile_deletes_dir_and_registry_entry() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let fake_claude = tmp.path().join(".claude");
    write_fake_claude_dir(&fake_claude, "del@test.com");
    mgr.add_profile_from("to-delete", &fake_claude).unwrap();

    mgr.remove_profile("to-delete").unwrap();

    assert!(!mgr.profile_dir("to-delete").exists());
    let reg = mgr.load_registry().unwrap();
    assert!(!reg.profiles.contains_key("to-delete"));
}

#[test]
fn test_remove_nonexistent_profile_errors() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);
    let err = mgr.remove_profile("ghost").unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

// ── get_profile ───────────────────────────────────────────────────────────────

#[test]
fn test_get_profile_returns_correct_entry() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    let fake_claude = tmp.path().join(".claude");
    write_fake_claude_dir(&fake_claude, "get@test.com");
    mgr.add_profile_from("found", &fake_claude).unwrap();

    let p = mgr.get_profile("found").unwrap();
    assert_eq!(p.name, "found");
    assert_eq!(p.email.as_deref(), Some("get@test.com"));
}

#[test]
fn test_get_profile_missing_errors() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);
    let err = mgr.get_profile("nope").unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
}

// ── profile_dir ───────────────────────────────────────────────────────────────

#[test]
fn test_profile_dir_path_is_correct() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);
    let expected = mgr.profiles_dir.join("myprofile");
    assert_eq!(mgr.profile_dir("myprofile"), expected);
}

// ── generate_aliases ──────────────────────────────────────────────────────────

#[test]
fn test_generate_aliases_empty() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);
    let out = mgr.generate_aliases().unwrap();
    assert!(out.contains("No profiles"), "{out}");
}

#[test]
fn test_generate_aliases_contains_all_profiles() {
    let tmp = TempDir::new().unwrap();
    let mgr = make_manager(&tmp);

    for name in &["work", "personal"] {
        let fake_claude = tmp.path().join(format!(".claude-{name}"));
        write_fake_claude_dir(&fake_claude, &format!("{name}@test.com"));
        mgr.add_profile_from(name, &fake_claude).unwrap();
    }

    let out = mgr.generate_aliases().unwrap();
    assert!(out.contains("alias claude-work="), "{out}");
    assert!(out.contains("alias claude-personal="), "{out}");
    assert!(out.contains("CLAUDE_CONFIG_DIR="), "{out}");
}
