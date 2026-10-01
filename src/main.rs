mod atomic;
mod codex;
mod gateway;
mod key;
mod limits;
mod profile;
mod skills_sync;
mod tui;
mod usage;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use limits::{Limits, Window, format_info, parse_limits, read_claude_json};
use profile::{LoginMethod, LoginOutcome, ProfileManager, Tool, detect_current_account};
use skills_sync::{SyncAction, SyncOptions, SyncReport};
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "cswitch",
    about = "Multi-account profile manager for Claude Code and Codex",
    long_about = "Manage Claude Code and Codex accounts using isolated profile directories.",
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
        /// Tool to log in to; Codex always starts a new login and cannot copy an account
        #[arg(long, value_enum, default_value_t = ToolChoice::Claude)]
        tool: ToolChoice,
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
        /// Authenticate using Anthropic Console API billing
        #[arg(long)]
        console: bool,
        /// Tool to log in to
        #[arg(long, value_enum, default_value_t = ToolChoice::Claude)]
        tool: ToolChoice,
    },

    /// Manage a profile's local Anthropic API key
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },

    /// Manage saved gateway settings
    Gateway {
        #[command(subcommand)]
        action: GatewayAction,
    },

    /// Remove a saved profile
    Remove {
        /// Profile name to remove
        name: String,
        /// Also delete this profile's token ledger rows
        #[arg(long)]
        purge_usage: bool,
    },

    /// Launch the selected profile's tool
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

    /// Read local Claude Code transcripts into an offline token ledger
    Usage {
        /// Range: 7d, 30d, all, or YYYY-MM-DD
        #[arg(long, default_value = "7d")]
        since: String,
        /// Only include one profile
        #[arg(long)]
        profile: Option<String>,
        /// Group by profile, workspace, project, session, model, or day
        #[arg(long, value_parser = ["profile", "workspace", "project", "session", "model", "day"])]
        by: Option<String>,
        /// Emit machine-readable report rows and footer
        #[arg(long)]
        json: bool,
        /// Show each attribution signal for a session
        #[arg(long, conflicts_with = "unattributed")]
        explain: Option<String>,
        /// Show sessions without a project and their candidates
        #[arg(long, conflicts_with = "explain")]
        unattributed: bool,
        #[command(subcommand)]
        action: Option<UsageAction>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ToolChoice {
    Claude,
    Codex,
}

#[derive(Subcommand)]
enum UsageAction {
    /// Save a project label for a session
    Label { session: String, project: String },
    /// Compare priced requests with matching cost-state snapshots
    Verify,
}

#[derive(Subcommand)]
enum KeyAction {
    /// Save a key read from hidden terminal input or one stdin line
    Set {
        name: String,
        #[arg(long)]
        replace_helper: bool,
    },
    /// Remove the saved key and cswitch-managed helper
    Clear { name: String },
    /// Print a saved key for Claude Code's apiKeyHelper
    #[command(hide = true)]
    Print { name: String },
}

#[derive(Subcommand)]
enum GatewayAction {
    /// List saved gateway URLs and setting names
    List,
    /// Forget defaults for a base URL
    Forget { url: String },
}

fn ask_save_defaults(base_dir: &Path, input: &gateway::GatewayInput) -> Result<bool> {
    let gateway::GatewayInput::Json {
        url,
        settings,
        dropped,
    } = input
    else {
        return Ok(false);
    };
    println!(
        "Read settings: {}.",
        settings.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    if !dropped.is_empty() {
        println!("Ignored credential names: {}.", dropped.join(", "));
    }
    let existing = gateway::read_defaults(base_dir)?;
    let replace = existing.get(url).is_some_and(|old| old != settings);
    let suffix = if replace {
        " (replaces the saved ones)"
    } else {
        ""
    };
    loop {
        print!(
            "Save these {} settings as the defaults for {url}? [Y/n]{suffix} ",
            settings.len()
        );
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            anyhow::bail!("Gateway defaults answer cancelled.");
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::try_parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    if let Some(Commands::Key {
        action: KeyAction::Print { name },
    }) = &cli.command
    {
        let base_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
            .join(".claude-switch");
        if let Err((code, phrase)) = key::print_key(
            &base_dir,
            name,
            io::stdout().is_terminal(),
            &mut io::stdout(),
        ) {
            eprintln!("{phrase}");
            std::process::exit(code);
        }
        return Ok(());
    }
    let manager = ProfileManager::new()?;

    match cli.command {
        None | Some(Commands::Ui) => {
            let app = tui::App::new(manager)?.with_executable(std::env::current_exe()?);
            app.run()?;
        }

        Some(Commands::List) => {
            print!("{}", list_output(&manager, Utc::now())?);
        }

        Some(Commands::Add {
            name,
            force,
            include_history,
            tool,
        }) => {
            if tool == ToolChoice::Codex {
                if force || include_history {
                    anyhow::bail!("Codex login does not support --force or --include-history.");
                }
                report_login(&name, &manager.login_codex_profile(&name)?);
            } else {
                handle_add(&manager, &name, force, include_history)?;
            }
        }

        Some(Commands::Login {
            name,
            include_history,
            email,
            console,
            tool,
        }) => {
            if tool == ToolChoice::Codex {
                if console || email.is_some() || include_history {
                    anyhow::bail!(
                        "Codex login does not support --console, --email or --include-history."
                    );
                }
                report_login(&name, &manager.login_codex_profile(&name)?);
                return Ok(());
            }
            let method = if console {
                LoginMethod::Console
            } else {
                LoginMethod::ClaudeAi
            };
            let outcome =
                manager.login_profile(&name, include_history, email.as_deref(), method)?;
            report_login(&name, &outcome);
        }

        Some(Commands::Key { action }) => match action {
            KeyAction::Set {
                name,
                replace_helper,
            } => {
                let terminal = io::stdin().is_terminal();
                let input = key::read_key_input()?;
                key::validate_key_input(&input)?;
                key::precheck_set_key(&manager, &name, replace_helper)?;
                let gateway_input = if terminal {
                    key::read_gateway_input(&key::gateway_display(&manager, &name))?
                } else {
                    gateway::GatewayInput::Keep
                };
                let save_defaults = ask_save_defaults(&manager.base_dir, &gateway_input)?;
                let executable = std::env::current_exe()?;
                let result = key::set_key_with_gateway(
                    &manager,
                    &name,
                    &input,
                    &executable,
                    replace_helper,
                    &gateway_input,
                    save_defaults,
                    Utc::now(),
                )?;
                println!("API key saved for profile '{name}'.");
                for line in result.gateway_lines {
                    println!("{line}");
                }
                if result.running_session {
                    println!("Restart this profile's running Claude sessions to use the key.");
                }
                if result.overrides_subscription {
                    println!("The key overrides the subscription; billing moves to the API.");
                }
                if result.build_path {
                    println!(
                        "Warning: this helper points at a build directory; install cswitch and rerun `key set`."
                    );
                }
            }
            KeyAction::Clear { name } => {
                let result = key::clear_key(&manager, &name, Utc::now())?;
                println!(
                    "API key removed for profile '{name}'. Fallback: {}.",
                    result.fallback
                );
                if result.foreign_helper {
                    println!("A foreign apiKeyHelper remains in settings.json.");
                }
                if let Some(line) = result.gateway_line {
                    println!("{line}");
                }
            }
            KeyAction::Print { .. } => unreachable!("handled before manager setup"),
        },

        Some(Commands::Gateway { action }) => match action {
            GatewayAction::List => print!("{}", gateway::list(&manager.base_dir)?),
            GatewayAction::Forget { url } => println!(
                "Forgot saved defaults for {}.",
                gateway::forget(&manager.base_dir, &url)?
            ),
        },

        Some(Commands::Remove { name, purge_usage }) => {
            if purge_usage {
                manager.get_profile(&name)?;
                let directory = std::env::var_os("CSWITCH_USAGE_DIR").map(std::path::PathBuf::from);
                usage::report::purge_profile(&usage::store(&manager, directory)?, &name)?;
            }
            match manager.remove_profile(&name) {
                Ok(_) => println!("Profile '{}' removed.", name),
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Use { name, args }) => {
            manager.launch_profile(&name, &args)?;
        }

        Some(Commands::Sync {
            name,
            all,
            dry_run,
            adopt,
        }) => {
            let opts = SyncOptions { dry_run, adopt };
            let names = sync_target_names(&manager, all, name)?;
            let home = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
            let (output, failed) = sync_profile_blocks(&manager, &names, &opts, &home, all)?;
            print!("{output}");
            if failed {
                std::process::exit(1);
            }
        }

        Some(Commands::Info { name }) => print!("{}", info_output(&manager, &name, Utc::now())?),

        Some(Commands::Aliases) => {
            println!("{}", manager.generate_aliases()?);
        }
        Some(Commands::Usage {
            since,
            profile,
            by,
            json,
            explain,
            unattributed,
            action,
        }) => {
            let directory = std::env::var_os("CSWITCH_USAGE_DIR").map(std::path::PathBuf::from);
            let store = usage::store(&manager, directory)?;
            let mut verify_exit_code = 0;
            let output = match action {
                Some(UsageAction::Label { session, project }) => {
                    usage::report::label(&store, &session, &project)?
                }
                Some(UsageAction::Verify) => {
                    let (output, exit_code) = usage::report::verify(&store)?;
                    verify_exit_code = exit_code;
                    output
                }
                None => usage::report::run(
                    &store,
                    &usage::report::Options {
                        since,
                        profile,
                        by,
                        json,
                        explain,
                        unattributed,
                    },
                    Utc::now(),
                )?,
            };
            print!("{output}");
            if verify_exit_code != 0 {
                std::process::exit(verify_exit_code);
            }
        }
    }

    Ok(())
}

fn info_output(manager: &ProfileManager, name: &str, now: DateTime<Utc>) -> Result<String> {
    let profile = manager.get_profile(name)?;
    let dir = manager.profile_dir(&profile.name);
    let mut output = format!(
        "Name:      {}\nTool:      {}\nEmail:     {}\n",
        profile.name,
        profile.tool.label(),
        profile.email.as_deref().unwrap_or("unknown")
    );
    if profile.tool == Tool::Claude {
        let claude = read_claude_json(&dir);
        let auth = key::read_auth_mode(manager, &profile.name, claude.clone());
        output.push_str(&format!(
            "Auth:      {}\nGateway:   {}\n",
            auth.label(),
            key::gateway_info(manager, &profile.name)
        ));
        let limits = claude
            .map(|json| json.map_or(Limits::Unreadable, |value| parse_limits(&value)))
            .unwrap_or(Limits::Unreadable);
        output.push_str(&format_info(&limits, now));
    } else {
        output.push_str("Auth:      —\nGateway:   —\n");
        if profile.tool == Tool::Codex {
            let plan = manager
                .codex_identity(&profile.name)
                .and_then(|identity| identity.plan_type)
                .unwrap_or_else(|| "—".into());
            output.push_str(&format!("Plan:      {plan}\n"));
        }
        output.push_str("Plan limits: —\n");
    }
    output.push_str(&format!(
        "Added:     {}\nLast used: {}\nDirectory: {}\n\nLaunch:\n  cswitch use {}\n",
        profile.added.format("%Y-%m-%d %H:%M UTC"),
        profile
            .last_used
            .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "never".to_string()),
        dir.display(),
        profile.name
    ));
    Ok(output)
}

fn list_output(manager: &ProfileManager, now: DateTime<Utc>) -> Result<String> {
    let profiles = manager.list_profiles()?;
    if profiles.is_empty() {
        return Ok("No profiles found. Add one with:\n  cswitch add <name>\n".to_string());
    }
    let header = format!(
        "{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}",
        "NAME", "TOOL", "EMAIL", "5H", "7D", "AS OF", "LAST USED"
    );
    let mut output = format!("{}\n{}\n", header, "─".repeat(header.chars().count()));
    let mut saw_reset = false;
    let mut saw_flag = false;
    let mut saw_api = false;

    for profile in profiles {
        let name: String = profile.name.chars().take(20).collect();
        let email: String = profile
            .email
            .as_deref()
            .unwrap_or("—")
            .chars()
            .take(30)
            .collect();
        let tool = profile.tool.label();
        let last_used = profile
            .last_used
            .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".to_string());
        if profile.tool != Tool::Claude {
            output.push_str(&format!(
                "{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n",
                name, tool, email, "—", "—", "—", last_used
            ));
            continue;
        }
        let dir = manager.profile_dir(&profile.name);
        let claude = read_claude_json(&dir);
        let auth = key::read_auth_mode(manager, &profile.name, claude.clone());
        let limits = claude
            .map(|json| json.map_or(Limits::Unreadable, |value| parse_limits(&value)))
            .unwrap_or(Limits::Unreadable);
        if auth.api_billed() {
            saw_api = true;
            output.push_str(&format!(
                "{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n",
                name,
                tool,
                email,
                "—",
                "—",
                "api",
                profile
                    .last_used
                    .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "never".to_string())
            ));
            continue;
        }
        let (session, weekly, age) = match &limits {
            Limits::Snapshot(snapshot) => {
                let session = list_window(snapshot.session(), now, false);
                let weekly = list_window(snapshot.weekly(), now, true);
                saw_reset |= session.1 || weekly.1;
                saw_flag |= session.2 || weekly.2;
                (session.0, weekly.0, snapshot.age(now))
            }
            Limits::NoSnapshot => ("—".to_string(), "—".to_string(), "no data".to_string()),
            Limits::AccountMismatch => ("—".to_string(), "—".to_string(), "mismatch".to_string()),
            Limits::Unreadable => ("—".to_string(), "—".to_string(), "unreadable".to_string()),
        };
        output.push_str(&format!(
            "{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n",
            name, tool, email, session, weekly, age, last_used
        ));
    }
    if saw_api {
        output.push_str("\napi = billed per token, no plan limits\n");
    }
    if saw_reset || saw_flag {
        let mut parts = Vec::new();
        if saw_reset {
            parts.push("reset = that window restarted after the snapshot was taken");
        }
        if saw_flag {
            parts.push("! = Claude Code flags this limit");
        }
        output.push_str(&format!("\n{}\n", parts.join(" · ")));
    }
    Ok(output)
}

fn list_window(
    window: Option<&Window>,
    now: DateTime<Utc>,
    with_bar: bool,
) -> (String, bool, bool) {
    let Some(window) = window else {
        return ("—".to_string(), false, false);
    };
    let reset = window.rolled_over(now);
    let flagged = window.flagged();
    let mut output = window.percent_label(now);
    if with_bar && !reset {
        output.push(' ');
        output.push_str(&window.bar());
    }
    if flagged {
        output.push_str(" !");
    }
    (output, reset, flagged)
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

fn sync_target_names(
    manager: &ProfileManager,
    all: bool,
    name: Option<String>,
) -> Result<Vec<String>> {
    if all {
        Ok(manager
            .list_profiles()?
            .into_iter()
            .filter(|profile| profile.tool == Tool::Claude)
            .map(|profile| profile.name)
            .collect())
    } else {
        Ok(vec![name.expect("clap requires a name or --all")])
    }
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
            println!("  [p]  Log in with the Anthropic Console (API billing)");
            println!();

            let choice = prompt_choice("Choice [c/l/p]: ", &['c', 'l', 'p'])?;

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
                    let outcome = manager.login_profile(
                        name,
                        include_history,
                        None,
                        LoginMethod::ClaudeAi,
                    )?;
                    report_login(name, &outcome);
                }
                'p' => {
                    let outcome =
                        manager.login_profile(name, include_history, None, LoginMethod::Console)?;
                    report_login(name, &outcome);
                }
                _ => unreachable!(),
            }
        }
        None => {
            // No active session — go straight to login
            println!("No active Claude session found. Opening Claude for login…\n");
            println!("  [l]  Log in with a Claude subscription");
            println!("  [p]  Log in with the Anthropic Console (API billing)");
            let method = if prompt_choice("Choice [l/p]: ", &['l', 'p'])? == 'p' {
                LoginMethod::Console
            } else {
                LoginMethod::ClaudeAi
            };
            let outcome = manager.login_profile(name, include_history, None, method)?;
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
    println!("\n{}", login_confirmation(name, outcome));

    let others: Vec<&str> = outcome
        .same_account_as
        .iter()
        .map(String::as_str)
        .filter(|n| *n != name)
        .collect();

    if !others.is_empty() {
        println!(
            "\n  Note: this is the same {} account as: {}",
            outcome.tool.label(),
            others.join(", ")
        );
        if outcome.tool == Tool::Claude {
            println!("  If you meant to add a different account, sign out of claude.ai");
            println!(
                "  (or use a private window) and run: cswitch remove {name} && cswitch login {name}"
            );
        } else if outcome.tool == Tool::Codex {
            println!("  To use a different Codex account, sign out of ChatGPT in the browser");
            println!(
                "  (or use a private window), then run: cswitch remove {name} && cswitch login {name} --tool codex"
            );
        }
    }

    println!("\nLaunch with: cswitch use {}", name);
}

fn login_confirmation(name: &str, outcome: &LoginOutcome) -> String {
    if outcome.tool == Tool::Codex && outcome.email.is_none() {
        format!("Codex login completed for profile '{name}' (email unavailable).")
    } else {
        format!(
            "Profile '{}' registered (account: {}).",
            name,
            outcome.display_email()
        )
    }
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
    use crate::profile::{Profile, Registry};
    use crate::skills_sync::SyncEntry;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn codex_login_without_identity_has_explicit_cli_confirmation() {
        // Known-bad: a successful login with unreadable claims reports an unnamed account.
        let outcome = LoginOutcome {
            email: None,
            same_account_as: Vec::new(),
            tool: Tool::Codex,
        };
        assert_eq!(
            login_confirmation("work", &outcome),
            "Codex login completed for profile 'work' (email unavailable)."
        );
    }

    #[test]
    fn console_and_key_cli_surfaces_parse_without_key_argument() {
        // Known-bad: dropping --console or accepting a secret as a positional argument.
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "login", "n", "--console"])
                .unwrap()
                .command,
            Some(Commands::Login { console: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "key", "set", "n", "--replace-helper"])
                .unwrap()
                .command,
            Some(Commands::Key {
                action: KeyAction::Set {
                    replace_helper: true,
                    ..
                }
            })
        ));
        assert!(Cli::try_parse_from(["cswitch", "key", "set", "n", "synthetic-secret"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "gateway", "list"])
                .unwrap()
                .command,
            Some(Commands::Gateway {
                action: GatewayAction::List
            })
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "cswitch",
                "gateway",
                "forget",
                "https://gateway.example.com"
            ])
            .unwrap()
            .command,
            Some(Commands::Gateway {
                action: GatewayAction::Forget { .. }
            })
        ));
    }

    #[test]
    fn g13_gateway_canary_never_reaches_views_or_non_key_files() {
        // Known-bad: a provider token gets copied from pasted JSON into settings, defaults or a backup.
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let manager = ProfileManager::with_paths(temp.path().join("base"), source.clone()).unwrap();
        manager.add_profile_from("api", &source).unwrap();
        fs::write(
            manager.profile_dir("api").join("settings.json"),
            r#"{"theme":"dark"}"#,
        )
        .unwrap();
        let executable = temp.path().join("cswitch");
        fs::write(&executable, "synthetic executable").unwrap();
        let key_canary = "TESTKEY-CANARY";
        let token_canary = "TOKEN-CANARY";
        let input = gateway::parse_gateway_input(&format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"https://gateway.example.com","ANTHROPIC_AUTH_TOKEN":"{token_canary}","ANTHROPIC_CUSTOM_HEADERS":"{token_canary}","ANTHROPIC_MODEL":"vendor/claude-model"}}}}"#)).unwrap();
        let outcome = key::set_key_with_gateway(
            &manager,
            "api",
            key_canary,
            &executable,
            false,
            &input,
            true,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(key::key_path(&manager.base_dir, "api")).unwrap(),
            format!("{key_canary}\n")
        );
        let info = format!(
            "{} {}",
            key::read_auth_mode(&manager, "api", Ok(None)).label(),
            key::gateway_info(&manager, "api")
        );
        let list = list_output(&manager, Utc::now()).unwrap();
        let gateways = gateway::list(&manager.base_dir).unwrap();
        for output in [outcome.gateway_lines.join("\n"), info, list, gateways] {
            assert!(!output.contains(token_canary));
            assert!(!output.contains(key_canary));
        }
        let clear = key::clear_key(&manager, "api", Utc::now()).unwrap();
        assert!(clear.gateway_line.unwrap().contains("gateway.example.com"));
        assert!(!key::key_path(&manager.base_dir, "api").exists());
        fn all_files(root: &Path, output: &mut Vec<u8>) {
            for entry in fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    all_files(&path, output);
                } else {
                    output.extend(fs::read(path).unwrap());
                }
            }
        }
        let mut files = Vec::new();
        all_files(&manager.base_dir, &mut files);
        assert!(
            !files
                .windows(token_canary.len())
                .any(|window| window == token_canary.as_bytes())
        );
        assert!(
            !files
                .windows(key_canary.len())
                .any(|window| window == key_canary.as_bytes())
        );
    }

    #[test]
    fn list_marks_api_billed_rows_and_keeps_width() {
        // Known-bad: API-billed profiles show no data instead of api.
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let manager = ProfileManager::with_paths(tmp.path().join("base"), source.clone()).unwrap();
        manager.add_profile_from("console", &source).unwrap();
        manager.add_profile_from("key", &source).unwrap();
        fs::write(
            manager.profile_dir("console").join(".claude.json"),
            r#"{"primaryApiKey":"synthetic"}"#,
        )
        .unwrap();
        fs::write(
            manager.profile_dir("key").join("settings.json"),
            r#"{"apiKeyHelper":"cswitch key print key"}"#,
        )
        .unwrap();
        fs::create_dir_all(manager.base_dir.join("keys")).unwrap();
        fs::write(key::key_path(&manager.base_dir, "key"), "synthetic\n").unwrap();
        let output = list_output(&manager, Utc::now()).unwrap();
        let rows: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("console") || line.starts_with("key "))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|line| line.split_whitespace().any(|field| field == "api"))
        );
        assert!(output.contains("api = billed per token, no plan limits"));
        assert!(output.lines().all(|line| line.chars().count() <= 120));
    }

    #[test]
    fn usage_cli_accepts_report_label_verify_and_purge_surfaces() {
        // Known-bad: a nested usage command consuming report flags or a remove
        // command that cannot opt into purging ledger rows.
        assert!(matches!(
            Cli::try_parse_from([
                "cswitch", "usage", "--since", "all", "--by", "model", "--json"
            ])
            .unwrap()
            .command,
            Some(Commands::Usage {
                by: Some(_),
                json: true,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "usage", "--explain", "sample"])
                .unwrap()
                .command,
            Some(Commands::Usage {
                explain: Some(_),
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "usage", "--unattributed"])
                .unwrap()
                .command,
            Some(Commands::Usage {
                unattributed: true,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "usage", "label", "sample", "blue/site"])
                .unwrap()
                .command,
            Some(Commands::Usage {
                action: Some(UsageAction::Label { .. }),
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "usage", "verify"])
                .unwrap()
                .command,
            Some(Commands::Usage {
                action: Some(UsageAction::Verify),
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "remove", "sample", "--purge-usage"])
                .unwrap()
                .command,
            Some(Commands::Remove {
                purge_usage: true,
                ..
            })
        ));
    }

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn list_uses_fixed_columns_and_only_needed_legend() {
        // Known-bad: always printing the legend, flagging normal limits, or overflowing at a 30-character email.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join(".claude-switch");
        let manager = ProfileManager::with_paths(base.clone(), tmp.path().join(".claude")).unwrap();
        let now = at("2030-01-07T14:00:00Z");
        let mut registry = Registry::default();
        for (name, email, last_used) in [
            (
                "active",
                "thirtyx.characters@example.com",
                Some(at("2030-01-07T06:31:00Z")),
            ),
            ("fresh", "b@example.com", None),
        ] {
            registry.profiles.insert(
                name.to_string(),
                Profile {
                    name: name.to_string(),
                    tool: Tool::Claude,
                    email: Some(email.to_string()),
                    added: now,
                    last_used,
                },
            );
            fs::create_dir_all(manager.profile_dir(name)).unwrap();
        }
        fs::write(
            base.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        let path = manager.profile_dir("active").join(".claude.json");
        let fresh_path = manager.profile_dir("fresh").join(".claude.json");
        fs::write(&fresh_path, b"{}").unwrap();
        let mut cache = serde_json::json!({
            "oauthAccount": {"accountUuid": "00000000-0000-4000-8000-000000000001"},
            "cachedUsageUtilization": {
                "accountUuid": "00000000-0000-4000-8000-000000000001",
                "fetchedAtMs": at("2030-01-07T13:00:00Z").timestamp_millis(),
                "utilization": {"limits": [
                    {"kind":"session", "group":"session", "percent":12,
                     "severity":"normal", "resets_at":"2030-01-07T15:00:00Z"},
                    {"kind":"weekly_all", "group":"weekly", "percent":88,
                     "severity":"normal", "resets_at":"2030-01-10T20:00:00Z"}
                ]}
            }
        });
        fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();

        let header = format!(
            "{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}",
            "NAME", "TOOL", "EMAIL", "5H", "7D", "AS OF", "LAST USED"
        );
        let expected = format!(
            "{header}\n{}\n{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n",
            "─".repeat(header.chars().count()),
            "active",
            "claude",
            "thirtyx.characters@example.com",
            "12%",
            "88% █████████░",
            "1 h ago",
            "2030-01-07 06:31",
            "fresh",
            "claude",
            "b@example.com",
            "—",
            "—",
            "no data",
            "never"
        );
        let output = list_output(&manager, now).unwrap();
        assert_eq!(output, expected);
        assert!(output.lines().all(|line| line.chars().count() <= 120));

        cache["cachedUsageUtilization"]["utilization"]["limits"][0]["resets_at"] =
            serde_json::json!("2030-01-07T12:00:00Z");
        cache["cachedUsageUtilization"]["utilization"]["limits"][1]["severity"] =
            serde_json::json!("critical");
        fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        let changed = list_output(&manager, now).unwrap();
        let expected_changed = format!(
            "{header}\n{}\n{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n{:<20} {:<12} {:<30} {:<7} {:<18} {:<11} {}\n\nreset = that window restarted after the snapshot was taken · ! = Claude Code flags this limit\n",
            "─".repeat(header.chars().count()),
            "active",
            "claude",
            "thirtyx.characters@example.com",
            "reset",
            "88% █████████░ !",
            "1 h ago",
            "2030-01-07 06:31",
            "fresh",
            "claude",
            "b@example.com",
            "—",
            "—",
            "no data",
            "never"
        );
        assert_eq!(changed, expected_changed);
        assert!(changed.lines().all(|line| line.chars().count() <= 120));

        cache["cachedUsageUtilization"]["accountUuid"] =
            serde_json::json!("00000000-0000-4000-8000-000000000002");
        fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        let mismatch = list_output(&manager, now).unwrap();
        let fields: Vec<&str> = mismatch
            .lines()
            .nth(2)
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(fields[3..6], ["—", "—", "mismatch"]);
        assert!(!mismatch.contains("88%"));
    }

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

    #[test]
    fn codex_add_and_login_flags_parse() {
        // Known-bad: --tool codex is missing from add or login.
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "add", "o", "--tool", "codex"])
                .unwrap()
                .command,
            Some(Commands::Add {
                tool: ToolChoice::Codex,
                ..
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cswitch", "login", "o", "--tool", "codex"])
                .unwrap()
                .command,
            Some(Commands::Login {
                tool: ToolChoice::Codex,
                ..
            })
        ));
    }

    fn mixed_tool_manager(tmp: &TempDir) -> ProfileManager {
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let mut registry = Registry::default();
        for (name, tool) in [
            ("c", Tool::Claude),
            ("o", Tool::Codex),
            ("u", Tool::Unknown("martian".into())),
        ] {
            registry.profiles.insert(
                name.into(),
                Profile {
                    name: name.into(),
                    tool: tool.clone(),
                    email: Some(format!("{}@example.com", "x".repeat(30))),
                    added: Utc::now(),
                    last_used: None,
                },
            );
            fs::create_dir_all(manager.profile_dir(name)).unwrap();
            if tool != Tool::Claude {
                fs::write(manager.profile_dir(name).join(".claude.json"), b"not JSON").unwrap();
            }
        }
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        manager
    }

    #[test]
    fn mixed_list_has_tool_column_and_skips_codex_limits() {
        // Known-bad: Codex list rows read Claude limits or exceed 120 columns with a 30-character email.
        let tmp = TempDir::new().unwrap();
        let manager = mixed_tool_manager(&tmp);
        let output = list_output(&manager, Utc::now()).unwrap();
        assert!(output.contains("o                    codex"), "{output}");
        assert!(
            output.contains("u                    unknown tool"),
            "{output}"
        );
        assert!(output.lines().all(|line| line.chars().count() <= 120));
        let codex_row = output.lines().find(|line| line.starts_with("o ")).unwrap();
        assert!(codex_row.contains("—       —"), "{codex_row}");
        assert!(!codex_row.contains("unreadable"), "{codex_row}");
    }

    #[test]
    fn info_uses_tool_specific_fields_and_keeps_claude_output() {
        // Known-bad: every known tool takes the Claude info branch and reads .claude.json.
        let tmp = TempDir::new().unwrap();
        let manager = mixed_tool_manager(&tmp);
        let auth = br#"{"tokens":{"id_token":"h.eyJlbWFpbCI6Im9AZXhhbXBsZS5jb20iLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9wbGFuX3R5cGUiOiJwbHVzIn19.s"}}"#;
        fs::write(manager.profile_dir("o").join("auth.json"), auth).unwrap();
        let now = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let codex = info_output(&manager, "o", now).unwrap();
        for expected in [
            "Tool:      codex\n",
            "Auth:      —\n",
            "Gateway:   —\n",
            "Plan:      plus\n",
            "Plan limits: —\n",
        ] {
            assert!(codex.contains(expected), "{codex}");
        }
        assert!(!codex.contains("unreadable"), "{codex}");

        let profile = manager.get_profile("c").unwrap();
        let claude = info_output(&manager, "c", now).unwrap();
        let expected = format!(
            "Name:      c\nTool:      claude\nEmail:     {}\nAuth:      not logged in\nGateway:   the Anthropic API\nPlan limits: the file couldn't be read.\nAdded:     {}\nLast used: never\nDirectory: {}\n\nLaunch:\n  cswitch use c\n",
            profile.email.unwrap(),
            profile.added.format("%Y-%m-%d %H:%M UTC"),
            manager.profile_dir("c").display()
        );
        assert_eq!(claude, expected);
    }

    #[test]
    fn sync_all_selects_only_claude_profiles() {
        // Known-bad: --all includes Codex homes in Claude skills sync.
        let tmp = TempDir::new().unwrap();
        let manager = mixed_tool_manager(&tmp);
        assert_eq!(sync_target_names(&manager, true, None).unwrap(), ["c"]);
    }
}
