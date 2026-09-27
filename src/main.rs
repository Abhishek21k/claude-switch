mod profile;
mod skills_sync;
mod tui;

use anyhow::Result;
use clap::{ArgGroup, Parser, Subcommand};
use profile::{LoginOutcome, ProfileManager, detect_current_account};
use skills_sync::{SyncAction, SyncOptions, SyncReport};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "cswitch",
    about = "Multi-account profile manager for Claude Code",
    long_about = "Manage multiple Claude Code accounts using isolated config directories.\n\
                  Each profile keeps its own credentials and settings, links shared skills,\n\
                  and launches Claude with CLAUDE_CONFIG_DIR set.",
    version,
    after_help = "\
Quick start:
  cswitch add work           Detect active session, copy or login
  cswitch login personal     Login to a different account
  cswitch use work           Launch Claude with a profile
  cswitch list               Show all profiles
  cswitch                    Open interactive TUI (press ? for help)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

impl Cli {
    fn try_parse_from<I, T>(input: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let raw: Vec<OsString> = input.into_iter().map(Into::into).collect();
        let mut cli = <Self as Parser>::try_parse_from(raw.clone())?;
        if let Some(Commands::Use { args, .. }) = &mut cli.command {
            // Clap consumes the `--` separator. Claude must receive it as typed.
            if let Some(use_index) = raw.iter().enumerate().skip(1).find_map(|(index, part)| {
                if part == "use" { Some(index) } else { None }
            }) {
                *args = raw.iter().skip(use_index + 2).cloned().collect();
            }
        }
        Ok(cli)
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Open the interactive TUI (default when no command given)
    Ui,

    /// List all saved profiles
    List,

    /// Add a new profile — detects active session and lets you choose
    Add {
        /// Profile name (alphanumeric, hyphens, underscores)
        name: String,
        /// Overwrite if profile already exists
        #[arg(short, long)]
        force: bool,
        /// Also copy conversation history and session transcripts.
        /// Off by default: separate sessions per profile are usually the point.
        #[arg(long)]
        include_history: bool,
    },

    /// Log in to a new Claude account and save it as a profile (skips detection prompt)
    Login {
        /// Profile name (alphanumeric, hyphens, underscores)
        name: String,
        /// Also copy conversation history and session transcripts.
        /// Off by default: separate sessions per profile are usually the point.
        #[arg(long)]
        include_history: bool,
        /// Pre-fill this address on Claude's login page. A convenience only —
        /// the account granted is whichever one the browser is signed in as.
        #[arg(long)]
        email: Option<String>,
    },

    /// Remove a saved profile
    Remove {
        /// Profile name to remove
        name: String,
    },

    /// Launch Claude Code with a specific profile
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Use {
        /// Profile name to use
        name: String,
        /// Extra arguments passed directly to claude
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },

    /// Link shared skills into one or all profiles
    #[command(group(ArgGroup::new("target").required(true).args(["name", "all"])))]
    Sync {
        /// Profile name
        name: Option<String>,
        /// Sync every registered profile
        #[arg(long)]
        all: bool,
        /// Preview changes without writing anything
        #[arg(long)]
        dry_run: bool,
        /// Back up and replace a diverged skill
        #[arg(long = "adopt")]
        adopt: Vec<String>,
    },

    /// Show details for a specific profile
    Info {
        /// Profile name
        name: String,
    },

    /// Print shell aliases for all profiles
    Aliases,
}

fn main() -> Result<()> {
    let cli = Cli::try_parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    let manager = ProfileManager::new()?;

    match cli.command {
        None | Some(Commands::Ui) => {
            let app = tui::App::new(manager)?;
            app.run()?;
        }

        Some(Commands::List) => {
            let profiles = manager.list_profiles()?;
            if profiles.is_empty() {
                println!("No profiles found. Add one with:");
                println!("  cswitch add <name>");
                return Ok(());
            }

            println!("{:<20} {:<35} LAST USED", "NAME", "EMAIL");
            println!("{}", "─".repeat(75));
            for p in profiles {
                let email = p.email.as_deref().unwrap_or("—");
                let last_used = p
                    .last_used
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or("never".to_string());
                println!("{:<20} {:<35} {}", p.name, email, last_used);
            }
        }

        Some(Commands::Add {
            name,
            force,
            include_history,
        }) => {
            handle_add(&manager, &name, force, include_history)?;
        }

        Some(Commands::Login {
            name,
            include_history,
            email,
        }) => {
            let outcome = manager.login_profile(&name, include_history, email.as_deref())?;
            report_login(&name, &outcome);
        }

        Some(Commands::Remove { name }) => match manager.remove_profile(&name) {
            Ok(_) => println!("Profile '{}' removed.", name),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },

        Some(Commands::Use { name, args }) => {
            manager.launch_claude(&name, &args)?;
        }

        Some(Commands::Sync {
            name,
            all,
            dry_run,
            adopt,
        }) => {
            let opts = SyncOptions { dry_run, adopt };
            let names = if all {
                manager
                    .list_profiles()?
                    .into_iter()
                    .map(|profile| profile.name)
                    .collect::<Vec<_>>()
            } else {
                vec![name.expect("clap requires a name or --all")]
            };
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
            let (output, failed) = sync_profile_blocks(&manager, &names, &opts, &home, all)?;
            print!("{output}");
            if failed {
                std::process::exit(1);
            }
        }

        Some(Commands::Info { name }) => match manager.get_profile(&name) {
            Ok(p) => {
                let dir = manager.profile_dir(&p.name);
                println!("Name:      {}", p.name);
                println!("Email:     {}", p.email.as_deref().unwrap_or("unknown"));
                println!("Added:     {}", p.added.format("%Y-%m-%d %H:%M UTC"));
                println!(
                    "Last used: {}",
                    p.last_used
                        .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or("never".to_string())
                );
                println!("Directory: {}", dir.display());
                println!();
                println!("Launch:");
                println!("  cswitch use {}", p.name);
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },

        Some(Commands::Aliases) => {
            println!("{}", manager.generate_aliases()?);
        }
    }

    Ok(())
}

fn sync_profile_blocks(
    manager: &ProfileManager,
    names: &[String],
    opts: &SyncOptions,
    home: &Path,
    all: bool,
) -> Result<(String, bool)> {
    let mut output = String::new();
    let mut failed = false;
    for name in names {
        match manager.sync_skills(name, opts) {
            Ok(report) => {
                output.push_str(&format_sync_report(name, &report, opts.dry_run, home));
                failed |= report.has_failures();
            }
            Err(error) if all => {
                output.push_str(&sync_header(name, opts.dry_run));
                output.push_str(&format!("\n  error: {error}\n"));
                failed = true;
            }
            Err(error) => return Err(error),
        }
    }
    Ok((output, failed))
}

fn sync_header(name: &str, dry_run: bool) -> String {
    if dry_run {
        format!("{name} (dry run):")
    } else {
        format!("{name}:")
    }
}

fn format_sync_report(name: &str, report: &SyncReport, dry_run: bool, home: &Path) -> String {
    let mut counts = [0usize; 9];
    let mut rows = Vec::<(&str, &str, Option<String>)>::new();
    for entry in &report.entries {
        let skill = &entry.name;
        let (index, label, detail) = match &entry.action {
            SyncAction::Linked => (0, if dry_run { "would link" } else { "linked" }, None),
            SyncAction::AlreadyLinked => {
                counts[1] += 1;
                continue;
            }
            SyncAction::Migrated { backup } => (
                2,
                if dry_run { "would migrate" } else { "migrated" },
                Some(format!("backup: {}", pretty_path(backup, home))),
            ),
            SyncAction::Adopted { backup } => (
                3,
                if dry_run { "would adopt" } else { "adopted" },
                Some(format!("backup: {}", pretty_path(backup, home))),
            ),
            SyncAction::Diverged => (
                4,
                "diverged",
                Some(format!(
                    "differs from {}; keep it, or run: cswitch sync {name} --adopt {skill}",
                    pretty_path(&home.join(".claude/skills").join(skill), home)
                )),
            ),
            SyncAction::ForeignLink { target } => (
                5,
                "foreign link",
                Some(format!("-> {}", pretty_path(target, home))),
            ),
            SyncAction::RemovedDangling => (
                6,
                if dry_run { "would remove" } else { "removed" },
                Some("dangling link into ~/.claude/skills".to_string()),
            ),
            SyncAction::ProfileOnly => (7, "profile-only", None),
            SyncAction::Failed { error } => (8, "FAILED", Some(error.clone())),
        };
        counts[index] += 1;
        rows.push((label, skill, detail));
    }
    let label_width = rows
        .iter()
        .map(|(label, _, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    let name_width = rows
        .iter()
        .map(|(_, skill, _)| skill.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![sync_header(name, dry_run)];
    for (label, skill, detail) in rows {
        lines.push(match detail {
            Some(detail) => {
                format!("  {label:<label_width$}  {skill:<name_width$}  {detail}")
            }
            None => format!("  {label:<label_width$}  {skill}"),
        });
    }
    lines.push(format!(
        "  counts: linked {}, already linked {}, migrated {}, adopted {}, diverged {}, foreign link {}, removed {}, profile-only {}, failed {}",
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4],
        counts[5],
        counts[6],
        counts[7],
        counts[8]
    ));
    lines.join("\n") + "\n"
}

fn pretty_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Smart add: detects an active Claude session and asks the user whether to
/// copy it or login fresh.
fn handle_add(
    manager: &ProfileManager,
    name: &str,
    force: bool,
    include_history: bool,
) -> Result<()> {
    match detect_current_account() {
        Some(acct) => {
            let email = acct.email.as_deref().unwrap_or("unknown");
            println!("Active Claude session detected: {}\n", email);
            println!("  [c]  Copy this session as profile '{}'", name);
            println!("  [l]  Login to a different account for profile '{}'", name);
            println!();

            let choice = prompt_choice("Choice [c/l]: ", &['c', 'l'])?;

            match choice {
                'c' => {
                    let result = if force {
                        manager.add_profile_force(name, include_history)
                    } else {
                        manager.add_profile(name, include_history)
                    };
                    match result {
                        Ok(p) => {
                            println!("\nProfile '{}' added.", p.name);
                            if let Some(email) = p.email {
                                println!("  Account: {}", email);
                            }
                            println!("  Launch with: cswitch use {}", p.name);
                        }
                        Err(e) => {
                            eprintln!("Error: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                'l' => {
                    let outcome = manager.login_profile(name, include_history, None)?;
                    report_login(name, &outcome);
                }
                _ => unreachable!(),
            }
        }
        None => {
            // No active session — go straight to login
            println!("No active Claude session found. Opening Claude for login…\n");
            let outcome = manager.login_profile(name, include_history, None)?;
            report_login(name, &outcome);
        }
    }
    Ok(())
}

/// Report a completed login using the account Claude actually authenticated as.
///
/// When the new profile landed on an account another profile already holds,
/// say so plainly — the usual cause is a browser that was still signed in, and
/// silently listing the email would look like a different account was added.
fn report_login(name: &str, outcome: &LoginOutcome) {
    println!(
        "\nProfile '{}' registered (account: {}).",
        name,
        outcome.display_email()
    );

    let others: Vec<&str> = outcome
        .same_account_as
        .iter()
        .map(String::as_str)
        .filter(|n| *n != name)
        .collect();

    if !others.is_empty() {
        println!(
            "\n  Note: this is the same Claude account as: {}",
            others.join(", ")
        );
        println!("  If you meant to add a different account, sign out of claude.ai");
        println!(
            "  (or use a private window) and run: cswitch remove {name} && cswitch login {name}"
        );
    }

    println!("\nLaunch with: cswitch use {}", name);
}

fn prompt_choice(prompt: &str, valid: &[char]) -> Result<char> {
    loop {
        print!("{}", prompt);
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        if let Some(c) = input.trim().chars().next() {
            let c = c.to_ascii_lowercase();
            if valid.contains(&c) {
                return Ok(c);
            }
        }
        println!(
            "Please enter one of: {}",
            valid
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_sync::SyncEntry;
    use std::fs;
    use tempfile::TempDir;

    fn use_args(input: &[&str]) -> Vec<String> {
        let cli = Cli::try_parse_from(input).unwrap();
        let Some(Commands::Use { name, args }) = cli.command else {
            panic!("expected use command");
        };
        assert_eq!(name, "personal");
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn use_passes_resume_flag_and_optional_id() {
        // Clap must not claim Claude's resume flag as a cswitch option.
        assert_eq!(
            use_args(&["cswitch", "use", "personal", "--resume"]),
            ["--resume"]
        );
        assert_eq!(
            use_args(&["cswitch", "use", "personal", "--resume", "session-id"]),
            ["--resume", "session-id"]
        );
    }

    #[test]
    fn use_passes_short_continue_flag() {
        // Treating -c as a cswitch option would consume Claude's flag.
        assert_eq!(use_args(&["cswitch", "use", "personal", "-c"]), ["-c"]);
    }

    #[test]
    fn use_passes_prompt_model_and_permission_flags() {
        // Parsing these as cswitch options would drop Claude's flag values.
        assert_eq!(
            use_args(&[
                "cswitch",
                "use",
                "personal",
                "-p",
                "two words",
                "--model",
                "opus",
                "--dangerously-skip-permissions",
            ]),
            [
                "-p",
                "two words",
                "--model",
                "opus",
                "--dangerously-skip-permissions"
            ]
        );
    }

    #[test]
    fn use_passes_double_dash_verbatim() {
        // Clap normally consumes its own argument separator.
        assert_eq!(
            use_args(&["cswitch", "use", "personal", "--", "-p", "two words"]),
            ["--", "-p", "two words"]
        );
    }

    #[test]
    fn use_passes_help_short_help_and_version_to_claude() {
        // Default help and version handlers would exit cswitch first.
        for flag in ["--help", "-h", "--version"] {
            assert_eq!(use_args(&["cswitch", "use", "personal", flag]), [flag]);
        }
    }

    #[test]
    fn sync_requires_exactly_one_profile_target() {
        // Two optional target arguments would accept neither or both.
        assert!(Cli::try_parse_from(["cswitch", "sync"]).is_err());
        assert!(Cli::try_parse_from(["cswitch", "sync", "personal", "--all"]).is_err());
        assert!(Cli::try_parse_from(["cswitch", "sync", "personal"]).is_ok());
        assert!(Cli::try_parse_from(["cswitch", "sync", "--all"]).is_ok());
    }

    #[test]
    fn sync_output_aligns_columns_within_each_profile_block() {
        // Fixed spaces between labels and details misalign mixed actions.
        let tmp = TempDir::new().unwrap();
        let report = SyncReport {
            entries: vec![
                SyncEntry {
                    name: "a".into(),
                    action: SyncAction::Linked,
                },
                SyncEntry {
                    name: "longer-name".into(),
                    action: SyncAction::Migrated {
                        backup: tmp.path().join("backup"),
                    },
                },
                SyncEntry {
                    name: "local".into(),
                    action: SyncAction::ProfileOnly,
                },
                SyncEntry {
                    name: "err".into(),
                    action: SyncAction::Failed {
                        error: "permission denied".into(),
                    },
                },
            ],
        };
        let output = format_sync_report("personal", &report, true, tmp.path());
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "personal (dry run):");
        assert_eq!(lines[1], format!("  {:<13}  a", "would link"));
        assert_eq!(
            lines[2],
            format!(
                "  {:<13}  {:<11}  backup: ~/backup",
                "would migrate", "longer-name"
            )
        );
        assert_eq!(lines[3], format!("  {:<13}  local", "profile-only"));
        assert_eq!(
            lines[4],
            format!("  {:<13}  {:<11}  permission denied", "FAILED", "err")
        );
        assert!(lines.iter().all(|line| !line.ends_with(' ')));
        assert_eq!(
            lines[5],
            "  counts: linked 1, already linked 0, migrated 1, adopted 0, diverged 0, foreign link 0, removed 0, profile-only 1, failed 1"
        );

        let short = SyncReport {
            entries: vec![SyncEntry {
                name: "z".into(),
                action: SyncAction::Linked,
            }],
        };
        let short_output = format_sync_report("work", &short, false, tmp.path());
        assert_eq!(short_output.lines().nth(1), Some("  linked  z"));
    }

    #[test]
    fn sync_all_reports_one_profile_error_and_continues_to_the_next() {
        // Propagating the first Err would skip later profiles.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let claude_home = home.join(".claude");
        let manager =
            ProfileManager::with_paths(home.join(".claude-switch"), claude_home.clone()).unwrap();
        let seed = home.join("seed");
        fs::create_dir_all(&seed).unwrap();
        manager.add_profile_from("bad", &seed).unwrap();
        manager.add_profile_from("good", &seed).unwrap();
        fs::create_dir_all(claude_home.join("skills")).unwrap();
        fs::write(claude_home.join("skills/alpha"), "shared").unwrap();
        fs::write(manager.profile_dir("bad").join("skills"), "blocked").unwrap();
        let opts = SyncOptions {
            dry_run: false,
            adopt: Vec::new(),
        };

        let (output, failed) =
            sync_profile_blocks(&manager, &["bad".into(), "good".into()], &opts, home, true)
                .unwrap();

        assert!(failed);
        assert!(output.starts_with("bad:\n  error: profile skills path is not a real directory\n"));
        assert!(output.contains("good:\n  linked  alpha\n"), "{output}");
        assert!(
            manager
                .profile_dir("good")
                .join("skills/alpha")
                .is_symlink()
        );
        assert!(sync_profile_blocks(&manager, &["bad".into()], &opts, home, false).is_err());
    }
}
