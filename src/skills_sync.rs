use anyhow::{Result, bail};
use chrono::Utc;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

pub struct SyncOptions {
    pub dry_run: bool,
    pub adopt: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SyncAction {
    Linked,
    AlreadyLinked,
    Migrated { backup: PathBuf },
    Adopted { backup: PathBuf },
    Diverged,
    ForeignLink { target: PathBuf },
    RemovedDangling,
    ProfileOnly,
    Failed { error: String },
}

#[derive(Debug, PartialEq, Eq)]
pub struct SyncEntry {
    pub name: String,
    pub action: SyncAction,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub entries: Vec<SyncEntry>,
}

impl SyncReport {
    pub fn has_failures(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.action, SyncAction::Failed { .. }))
    }
}

/// Reconcile one profile's skills against explicit paths. The source is read only.
pub fn sync_skills(
    source: &Path,
    profile_skills: &Path,
    backup_dir: &Path,
    opts: &SyncOptions,
) -> Result<SyncReport> {
    if !source.is_absolute() {
        bail!("skills source must be an absolute path");
    }
    match fs::metadata(source) {
        Ok(meta) if !meta.is_dir() => bail!("skills source is not a directory"),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(SyncReport::default()),
        Err(e) => return Err(e.into()),
    }
    let source_canonical = fs::canonicalize(source)?;
    if resolved_destination(profile_skills)?.starts_with(&source_canonical)
        || resolved_destination(backup_dir)?.starts_with(&source_canonical)
    {
        bail!("skills destination or backup is inside the source");
    }
    match fs::symlink_metadata(profile_skills) {
        Ok(meta) if !meta.is_dir() => bail!("profile skills path is not a real directory"),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    let mut source_entries = BTreeMap::<OsString, PathBuf>::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if eligible(&name) {
            source_entries.insert(name, entry.path());
        }
    }
    for name in &opts.adopt {
        if !source_entries.contains_key(OsStr::new(name)) {
            bail!("Cannot adopt '{name}': it is not a source skill");
        }
    }

    if !opts.dry_run {
        fs::create_dir_all(profile_skills)?;
    }
    let mut report = SyncReport::default();
    for (name, source_entry) in &source_entries {
        let dest = profile_skills.join(name);
        let action = sync_source_entry(
            source_entry,
            &dest,
            backup_dir,
            opts.adopt.iter().any(|adopt| OsStr::new(adopt) == name),
            opts.dry_run,
            false,
        );
        report.entries.push(SyncEntry {
            name: name.to_string_lossy().into_owned(),
            action,
        });
    }

    if profile_skills.is_dir() {
        let normalized_source = normalize(source);
        let source_names: HashSet<&OsString> = source_entries.keys().collect();
        for entry in fs::read_dir(profile_skills)? {
            let entry = entry?;
            let name = entry.file_name();
            if !eligible(&name) || source_names.contains(&name) {
                continue;
            }
            let path = entry.path();
            let action = profile_only_action(&path, &normalized_source, opts.dry_run);
            report.entries.push(SyncEntry {
                name: name.to_string_lossy().into_owned(),
                action,
            });
        }
    }
    report.entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(report)
}

fn eligible(name: &OsStr) -> bool {
    name != "synced" && !name.to_string_lossy().starts_with('.')
}

fn resolved_destination(path: &Path) -> io::Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or(e)?;
                suffix.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "path has no existing parent")
                })?;
            }
            Err(e) => return Err(e),
        }
    }
    let mut resolved = fs::canonicalize(existing)?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

fn sync_source_entry(
    source: &Path,
    dest: &Path,
    backup_dir: &Path,
    adopt: bool,
    dry_run: bool,
    locked: bool,
) -> SyncAction {
    let meta = match fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if dry_run {
                return SyncAction::Linked;
            }
            return match create_link(source, dest) {
                Ok(()) => SyncAction::Linked,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => recheck_link(source, dest, e),
                Err(e) => failed(e),
            };
        }
        Err(e) => return failed(e),
    };
    if meta.file_type().is_symlink() {
        return match fs::read_link(dest) {
            Ok(raw) if resolved_target(dest, &raw) == normalize(source) => {
                SyncAction::AlreadyLinked
            }
            Ok(raw) => SyncAction::ForeignLink { target: raw },
            Err(e) => failed(e),
        };
    }

    let identical = match byte_identical(source, dest) {
        Ok(equal) => equal,
        Err(e) => return failed(e),
    };
    if !identical && !adopt {
        return SyncAction::Diverged;
    }
    if !dry_run && !locked {
        let Some(parent) = backup_dir.parent() else {
            return SyncAction::Failed {
                error: "backup directory has no parent".to_string(),
            };
        };
        if let Err(e) = fs::create_dir_all(parent) {
            return failed(e);
        }
        let lock_path = parent.join(format!(
            ".{}.lock",
            backup_dir.file_name().unwrap().to_string_lossy()
        ));
        let lock = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
        {
            Ok(lock) => lock,
            Err(e) => return failed(e),
        };
        if let Err(e) = lock.lock() {
            return failed(e);
        }
        // A concurrent launch may have migrated this entry while we waited.
        return sync_source_entry(source, dest, backup_dir, adopt, false, true);
    }
    let backup = match available_backup_path(backup_dir, dest.file_name().unwrap()) {
        Ok(path) => path,
        Err(e) => return failed(e),
    };
    if dry_run {
        return if identical {
            SyncAction::Migrated { backup }
        } else {
            SyncAction::Adopted { backup }
        };
    }
    if let Err(e) = fs::create_dir_all(backup_dir) {
        return failed(e);
    }
    if let Err(e) = fs::rename(dest, &backup) {
        let _ = fs::remove_dir(backup_dir);
        if e.kind() == io::ErrorKind::NotFound {
            return recheck_link(source, dest, e);
        }
        return failed(e);
    }
    match create_link(source, dest) {
        Ok(()) => {
            if identical {
                SyncAction::Migrated { backup }
            } else {
                SyncAction::Adopted { backup }
            }
        }
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists
                && matches!(link_is_correct(source, dest), Ok(true)) =>
        {
            SyncAction::AlreadyLinked
        }
        Err(e) => match fs::rename(&backup, dest) {
            Ok(()) => {
                let _ = fs::remove_dir(backup_dir);
                failed(e)
            }
            Err(rollback) => SyncAction::Failed {
                error: format!("{e}; restoring backup failed: {rollback}"),
            },
        },
    }
}

fn recheck_link(source: &Path, dest: &Path, original: io::Error) -> SyncAction {
    match link_is_correct(source, dest) {
        Ok(true) => SyncAction::AlreadyLinked,
        Ok(false) => failed(original),
        Err(e) => failed(e),
    }
}

fn link_is_correct(source: &Path, dest: &Path) -> io::Result<bool> {
    let meta = fs::symlink_metadata(dest)?;
    if !meta.file_type().is_symlink() {
        return Ok(false);
    }
    Ok(resolved_target(dest, &fs::read_link(dest)?) == normalize(source))
}

fn profile_only_action(path: &Path, source: &Path, dry_run: bool) -> SyncAction {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) => return failed(e),
    };
    if !meta.file_type().is_symlink() {
        return SyncAction::ProfileOnly;
    }
    let raw = match fs::read_link(path) {
        Ok(raw) => raw,
        Err(e) => return failed(e),
    };
    let target = resolved_target(path, &raw);
    if target.starts_with(source) && !target.exists() {
        if dry_run {
            return SyncAction::RemovedDangling;
        }
        return match fs::remove_file(path) {
            Ok(()) => SyncAction::RemovedDangling,
            Err(e) if e.kind() == io::ErrorKind::NotFound => SyncAction::ProfileOnly,
            Err(e) => failed(e),
        };
    }
    SyncAction::ProfileOnly
}

fn available_backup_path(dir: &Path, name: &OsStr) -> io::Result<PathBuf> {
    let mut base = name.to_os_string();
    base.push(format!("-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));
    for index in 1.. {
        let mut candidate_name = base.clone();
        if index > 1 {
            candidate_name.push(format!("-{index}"));
        }
        let candidate = dir.join(candidate_name);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

fn byte_identical(a: &Path, b: &Path) -> io::Result<bool> {
    let left = fs::symlink_metadata(a)?.file_type();
    let right = fs::symlink_metadata(b)?.file_type();
    if left.is_symlink() || right.is_symlink() {
        return Ok(left.is_symlink()
            && right.is_symlink()
            && fs::read_link(a)? == fs::read_link(b)?);
    }
    if left.is_file() || right.is_file() {
        return Ok(left.is_file() && right.is_file() && fs::read(a)? == fs::read(b)?);
    }
    if !left.is_dir() || !right.is_dir() {
        return Ok(false);
    }
    let mut left_entries = BTreeMap::new();
    let mut right_entries = BTreeMap::new();
    for entry in fs::read_dir(a)? {
        let entry = entry?;
        left_entries.insert(entry.file_name(), entry.path());
    }
    for entry in fs::read_dir(b)? {
        let entry = entry?;
        right_entries.insert(entry.file_name(), entry.path());
    }
    if left_entries.len() != right_entries.len() {
        return Ok(false);
    }
    for (name, left_path) in &left_entries {
        let Some(right_path) = right_entries.get(name) else {
            return Ok(false);
        };
        if !byte_identical(left_path, right_path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn resolved_target(link: &Path, raw: &Path) -> PathBuf {
    if raw.is_absolute() {
        normalize(raw)
    } else {
        normalize(&link.parent().unwrap_or(Path::new(".")).join(raw))
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn failed(error: io::Error) -> SyncAction {
    SyncAction::Failed {
        error: error.to_string(),
    }
}

#[cfg(unix)]
fn create_link(source: &Path, dest: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use tempfile::TempDir;

    fn paths(tmp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        (
            tmp.path().join("source"),
            tmp.path().join("profile/skills"),
            tmp.path().join("backups/skills/work"),
        )
    }

    fn options() -> SyncOptions {
        SyncOptions {
            dry_run: false,
            adopt: Vec::new(),
        }
    }

    fn action<'a>(report: &'a SyncReport, name: &str) -> &'a SyncAction {
        &report
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
            .action
    }

    fn hash_tree(path: &Path) -> u64 {
        fn visit(path: &Path, relative: &Path, hasher: &mut DefaultHasher) {
            relative.hash(hasher);
            let kind = fs::symlink_metadata(path).unwrap().file_type();
            if kind.is_symlink() {
                "link".hash(hasher);
                fs::read_link(path).unwrap().hash(hasher);
            } else if kind.is_file() {
                "file".hash(hasher);
                fs::read(path).unwrap().hash(hasher);
            } else {
                "dir".hash(hasher);
                let mut children = fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>();
                children.sort();
                for name in children {
                    visit(&path.join(&name), &relative.join(name), hasher);
                }
            }
        }
        let mut hasher = DefaultHasher::new();
        visit(path, Path::new(""), &mut hasher);
        hasher.finish()
    }

    #[test]
    fn synced_tree_with_nested_and_zero_byte_marker_survives_sync() {
        // Linking the whole skills folder would replace this account-managed tree.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(source.join("new-skill")).unwrap();
        fs::create_dir_all(profile.join("synced/bucket/nested")).unwrap();
        fs::write(profile.join("synced/bucket/nested/data.txt"), b"content").unwrap();
        fs::write(profile.join("synced/bucket/.bucket-<id>"), b"").unwrap();
        let before = hash_tree(&profile.join("synced"));

        sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(before, hash_tree(&profile.join("synced")));
        assert!(profile.join("new-skill").is_symlink());
    }

    #[test]
    fn diverged_copy_survives_instead_of_being_replaced() {
        // Replacing every copy with a link would lose local edits.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(source.join("skill/nested")).unwrap();
        fs::create_dir_all(profile.join("skill/nested")).unwrap();
        fs::write(source.join("skill/nested/SKILL.md"), "source").unwrap();
        fs::write(profile.join("skill/nested/SKILL.md"), "local").unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(action(&report, "skill"), &SyncAction::Diverged);
        assert_eq!(
            fs::read_to_string(profile.join("skill/nested/SKILL.md")).unwrap(),
            "local"
        );
        assert!(!backup.exists());
    }

    #[test]
    fn identical_copy_is_backed_up_before_linking() {
        // Replacing an identical copy without a backup would lose recoverability.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(source.join("skill/nested")).unwrap();
        fs::create_dir_all(profile.join("skill/nested")).unwrap();
        fs::write(source.join("skill/nested/SKILL.md"), b"same").unwrap();
        fs::write(profile.join("skill/nested/SKILL.md"), b"same").unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        let SyncAction::Migrated { backup } = action(&report, "skill") else {
            panic!("expected migration");
        };
        assert_eq!(
            fs::read_to_string(backup.join("nested/SKILL.md")).unwrap(),
            "same"
        );
        assert_eq!(
            fs::read_link(profile.join("skill")).unwrap(),
            source.join("skill")
        );
    }

    #[test]
    fn adopt_backs_up_divergence_and_unknown_adopt_changes_nothing() {
        // Accepting an unknown name after linking other skills would leave partial work.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(source.join("skill"), "source").unwrap();
        fs::write(profile.join("skill"), "local").unwrap();
        let before = hash_tree(tmp.path());
        let invalid = SyncOptions {
            dry_run: false,
            adopt: vec!["unknown".into()],
        };
        assert!(sync_skills(&source, &profile, &backup, &invalid).is_err());
        assert_eq!(before, hash_tree(tmp.path()));

        let valid = SyncOptions {
            dry_run: false,
            adopt: vec!["skill".into()],
        };
        let report = sync_skills(&source, &profile, &backup, &valid).unwrap();
        let SyncAction::Adopted { backup } = action(&report, "skill") else {
            panic!("expected adoption");
        };
        assert_eq!(fs::read_to_string(backup).unwrap(), "local");
        assert_eq!(
            fs::read_link(profile.join("skill")).unwrap(),
            source.join("skill")
        );
    }

    #[test]
    fn second_run_has_no_writes_or_change_actions() {
        // An unconditional relink would churn profile state every launch.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("skill"), "content").unwrap();
        sync_skills(&source, &profile, &backup, &options()).unwrap();
        let before = hash_tree(tmp.path());

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(before, hash_tree(tmp.path()));
        assert_eq!(action(&report, "skill"), &SyncAction::AlreadyLinked);
        assert!(!report.entries.iter().any(|entry| matches!(
            entry.action,
            SyncAction::Linked | SyncAction::Migrated { .. } | SyncAction::RemovedDangling
        )));
    }

    #[test]
    #[cfg(unix)]
    fn only_dangling_links_into_source_are_removed() {
        // Removing every dangling link would delete unrelated profile entries.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profile).unwrap();
        std::os::unix::fs::symlink(source.join("removed"), profile.join("removed")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("foreign"), profile.join("foreign")).unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(action(&report, "removed"), &SyncAction::RemovedDangling);
        assert_eq!(action(&report, "foreign"), &SyncAction::ProfileOnly);
        assert!(fs::symlink_metadata(profile.join("removed")).is_err());
        assert!(profile.join("foreign").is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn missing_source_never_cleans_dangling_links() {
        // Treating an unavailable source as an empty installation would delete links.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&profile).unwrap();
        std::os::unix::fs::symlink(source.join("skill"), profile.join("skill")).unwrap();
        let before = hash_tree(tmp.path());

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert!(report.entries.is_empty());
        assert_eq!(before, hash_tree(tmp.path()));
    }

    #[test]
    #[cfg(unix)]
    fn dry_run_reports_actions_without_creating_dirs_or_backups() {
        // A preview that performs migrations would be unsafe for review.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(source.join("copy"), "same").unwrap();
        fs::write(profile.join("copy"), "same").unwrap();
        fs::write(source.join("new"), "new").unwrap();
        std::os::unix::fs::symlink(source.join("gone"), profile.join("gone")).unwrap();
        let before = hash_tree(tmp.path());
        let opts = SyncOptions {
            dry_run: true,
            adopt: Vec::new(),
        };

        let report = sync_skills(&source, &profile, &backup, &opts).unwrap();

        assert!(matches!(
            action(&report, "copy"),
            SyncAction::Migrated { .. }
        ));
        assert_eq!(action(&report, "new"), &SyncAction::Linked);
        assert_eq!(action(&report, "gone"), &SyncAction::RemovedDangling);
        assert_eq!(before, hash_tree(tmp.path()));
        assert!(!backup.exists());
    }

    #[test]
    #[cfg(unix)]
    fn source_symlink_is_linked_by_entry_path_not_repo_target() {
        // Canonicalizing would bypass the source entry and break future relinks.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        let repo = tmp.path().join("repo/skill");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("SKILL.md"), "live").unwrap();
        std::os::unix::fs::symlink(&repo, source.join("skill")).unwrap();

        sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(
            fs::read_link(profile.join("skill")).unwrap(),
            source.join("skill")
        );
        assert_eq!(
            fs::read_to_string(profile.join("skill/SKILL.md")).unwrap(),
            "live"
        );
    }

    #[test]
    fn dotfiles_and_synced_are_never_linked() {
        // A broad directory scan would import per-account synced skills.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(source.join("synced/bucket")).unwrap();
        fs::write(source.join(".hidden"), "hidden").unwrap();
        fs::write(source.join("skill"), "shared").unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(report.entries.len(), 1);
        assert_eq!(action(&report, "skill"), &SyncAction::Linked);
        assert!(!profile.join(".hidden").exists());
        assert!(!profile.join("synced").exists());
    }

    #[test]
    #[cfg(unix)]
    fn relative_link_to_source_entry_is_already_linked() {
        // Comparing raw read_link paths would mark a valid relative link foreign.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(source.join("skill"), "shared").unwrap();
        std::os::unix::fs::symlink("../../source/./skill", profile.join("skill")).unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(action(&report, "skill"), &SyncAction::AlreadyLinked);
    }

    #[test]
    fn same_bytes_with_extra_relative_path_is_diverged() {
        // Comparing only SKILL.md bytes would miss another file in the copy.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(source.join("skill")).unwrap();
        fs::create_dir_all(profile.join("skill")).unwrap();
        fs::write(source.join("skill/SKILL.md"), "same").unwrap();
        fs::write(profile.join("skill/SKILL.md"), "same").unwrap();
        fs::write(profile.join("skill/local.txt"), "extra").unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(action(&report, "skill"), &SyncAction::Diverged);
    }

    #[test]
    #[cfg(unix)]
    fn identical_tree_requires_matching_raw_symlink_targets() {
        // Following symlinks would call distinct link targets identical.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(source.join("skill")).unwrap();
        fs::create_dir_all(profile.join("skill")).unwrap();
        fs::write(tmp.path().join("target"), "same").unwrap();
        std::os::unix::fs::symlink("../../target", source.join("skill/link")).unwrap();
        std::os::unix::fs::symlink("../../../target", profile.join("skill/link")).unwrap();

        let report = sync_skills(&source, &profile, &backup, &options()).unwrap();

        assert_eq!(action(&report, "skill"), &SyncAction::Diverged);
        assert!(!backup.exists());
    }

    #[test]
    #[cfg(unix)]
    fn whole_folder_symlink_is_refused_without_touching_source() {
        // Traversing a profile-wide link would write into the read-only source.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(profile.parent().unwrap()).unwrap();
        fs::write(source.join("skill"), "source").unwrap();
        std::os::unix::fs::symlink(&source, &profile).unwrap();
        let before = hash_tree(&source);

        assert!(sync_skills(&source, &profile, &backup, &options()).is_err());

        assert_eq!(before, hash_tree(&source));
        assert!(profile.is_symlink());
    }

    #[test]
    fn concurrent_migrations_leave_one_backup_and_one_correct_link() {
        // Two launches must not overwrite one another's timestamped backup.
        let tmp = TempDir::new().unwrap();
        let (source, profile, backup) = paths(&tmp);
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(source.join("skill"), "same").unwrap();
        fs::write(profile.join("skill"), "same").unwrap();
        std::thread::scope(|scope| {
            let a = scope.spawn(|| sync_skills(&source, &profile, &backup, &options()).unwrap());
            let b = scope.spawn(|| sync_skills(&source, &profile, &backup, &options()).unwrap());
            let reports = [a.join().unwrap(), b.join().unwrap()];
            assert!(
                reports
                    .iter()
                    .any(|r| matches!(action(r, "skill"), SyncAction::Migrated { .. }))
            );
            assert!(
                reports
                    .iter()
                    .any(|r| action(r, "skill") == &SyncAction::AlreadyLinked)
            );
        });
        assert_eq!(
            fs::read_link(profile.join("skill")).unwrap(),
            source.join("skill")
        );
        assert_eq!(fs::read_dir(&backup).unwrap().count(), 1);
    }
}

#[cfg(windows)]
fn create_link(source: &Path, dest: &Path) -> io::Result<()> {
    if fs::metadata(source).is_ok_and(|meta| meta.is_dir()) {
        std::os::windows::fs::symlink_dir(source, dest)
    } else {
        std::os::windows::fs::symlink_file(source, dest)
    }
}
