mod agy;
mod atomic;
mod codex;
mod gateway;
mod key;
mod limits;
mod profile;
mod skills_sync;
mod statusline;
mod tui;
mod usage;

use anyhow::Result;
use chrono::{DateTime, FixedOffset, Local, Offset, Utc};
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
    about = "Multi-account profile manager for Claude Code, Codex and Antigravity",
    long_about = "Manage Claude Code, Codex and Antigravity accounts using isolated profile directories.",
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
        /// Tool to log in to; Codex and Antigravity always start a new login
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
        /// Also delete this profile's token ledger rows, billing settings and limit history
        #[arg(long)]
        purge_usage: bool,
        /// Confirm deletion of local files in an Antigravity profile's HOME
        #[arg(long)]
        force: bool,
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

    /// Print or install a Claude Code status line
    #[command(group(ArgGroup::new("statusline_action").args(["install", "uninstall"])))]
    #[command(group(ArgGroup::new("statusline_target").args(["name", "all"])))]
    Statusline {
        /// Install the command in a profile's settings.json
        #[arg(long, requires = "statusline_target")]
        install: bool,
        /// Remove a cswitch-owned status line
        #[arg(long, requires = "statusline_target")]
        uninstall: bool,
        /// Registered Claude profile
        #[arg(requires = "statusline_action")]
        name: Option<String>,
        /// Apply to every registered Claude profile
        #[arg(long, requires = "statusline_action")]
        all: bool,
        /// Replace a status line owned by another command
        #[arg(long, requires = "install", conflicts_with = "uninstall")]
        force: bool,
        /// Render without a background ledger refresh
        #[arg(long, conflicts_with = "uninstall")]
        no_refresh: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ToolChoice {
    Claude,
    Codex,
    #[value(alias = "agy")]
    Antigravity,
}

#[derive(Subcommand)]
enum UsageAction {
    /// Refresh the local usage ledger without printing a report
    Refresh,
    /// Save a project label for a session
    Label { session: String, project: String },
    /// Compare priced requests with matching cost-state snapshots
    Verify,
    /// Map a transcript model to a list-price model
    Alias {
        model: String,
        rates_model_id: Option<String>,
        #[arg(long)]
        remove: bool,
    },
    /// Set a subscription plan fee
    Plan {
        profile: String,
        fee_usd: Option<f64>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Set per-token prices in USD per million tokens
    Rate {
        profile: String,
        #[arg(long, conflicts_with_all = ["input", "output", "cache_write_5m", "cache_write_1h", "cache_read", "clear"])]
        flat: Option<f64>,
        #[arg(long)]
        input: Option<f64>,
        #[arg(long)]
        output: Option<f64>,
        #[arg(long)]
        cache_write_5m: Option<f64>,
        #[arg(long)]
        cache_write_1h: Option<f64>,
        #[arg(long)]
        cache_read: Option<f64>,
        #[arg(long = "model-prefix")]
        model_prefix: Vec<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Record, list or undo a free weekly plan-limit reset
    Reset {
        profile: String,
        /// Local day (YYYY-MM-DD) or minute (YYYY-MM-DD HH:MM)
        #[arg(long, conflicts_with_all = ["list", "undo"])]
        at: Option<String>,
        #[arg(long, conflicts_with = "undo")]
        list: bool,
        #[arg(long)]
        undo: bool,
    },
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
    if let Some(Commands::Statusline {
        install: false,
        uninstall: false,
        no_refresh,
        ..
    }) = &cli.command
    {
        std::panic::set_hook(Box::new(|_| {}));
        let output = std::panic::catch_unwind(|| statusline::command_line(*no_refresh))
            .unwrap_or_else(|_| statusline::CommandOutput::fallback());
        statusline::emit_and_refresh(output, &mut io::stdout(), statusline::spawn_refresh);
        return Ok(());
    }
    if let Some(Commands::Statusline {
        install,
        name,
        all,
        force,
        no_refresh,
        ..
    }) = &cli.command
    {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
        let manager = ProfileManager::with_paths_read_only(
            home.join(".claude-switch"),
            home.join(".claude"),
        )?;
        let action = if *install {
            statusline::Action::Install {
                no_refresh: *no_refresh,
                force: *force,
            }
        } else {
            statusline::Action::Uninstall
        };
        let (output, failed) = statusline::manage(
            &manager,
            name.as_deref(),
            *all,
            action,
            &std::env::current_exe()?,
            Utc::now(),
        )?;
        print!("{output}");
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }
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
        }) => match tool {
            ToolChoice::Claude => handle_add(&manager, &name, force, include_history)?,
            ToolChoice::Codex | ToolChoice::Antigravity => {
                if force || include_history {
                    anyhow::bail!("This login does not support --force or --include-history.");
                }
                let outcome = match tool {
                    ToolChoice::Codex => manager.login_codex_profile(&name)?,
                    ToolChoice::Antigravity => manager.login_agy_profile(&name)?,
                    ToolChoice::Claude => unreachable!(),
                };
                report_login(&name, &outcome);
            }
        },

        Some(Commands::Login {
            name,
            include_history,
            email,
            console,
            tool,
        }) => {
            if tool != ToolChoice::Claude {
                if console || email.is_some() || include_history {
                    anyhow::bail!(
                        "This login does not support --console, --email or --include-history."
                    );
                }
                let outcome = match tool {
                    ToolChoice::Codex => manager.login_codex_profile(&name)?,
                    ToolChoice::Antigravity => manager.login_agy_profile(&name)?,
                    ToolChoice::Claude => unreachable!(),
                };
                report_login(&name, &outcome);
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

        Some(Commands::Remove {
            name,
            purge_usage,
            force,
        }) => {
            let directory = std::env::var_os("CSWITCH_USAGE_DIR").map(std::path::PathBuf::from);
            match remove_profile_with_usage(&manager, &name, purge_usage, force, directory) {
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
                Some(UsageAction::Refresh) => {
                    store.ingest()?;
                    String::new()
                }
                Some(UsageAction::Label { session, project }) => {
                    usage::report::label(&store, &session, &project)?
                }
                Some(UsageAction::Alias {
                    model,
                    rates_model_id,
                    remove,
                }) => {
                    let Some(_lock) = store.try_lock()? else {
                        anyhow::bail!("usage store busy; try again")
                    };
                    if remove {
                        if rates_model_id.is_some() {
                            anyhow::bail!("--remove takes no rates model id")
                        }
                    } else if rates_model_id.is_none() {
                        anyhow::bail!("provide a rates model id or --remove")
                    }
                    let changed =
                        usage::rates::edit_alias(&store.dir, &model, rates_model_id.as_deref())?;
                    if changed {
                        format!("Alias updated for {model}.\n")
                    } else {
                        format!("No alias for {model}; nothing changed.\n")
                    }
                }
                Some(UsageAction::Plan {
                    profile,
                    fee_usd,
                    label,
                    clear,
                }) => {
                    require_billing_class(&manager, &profile, false, "plans")?;
                    if clear {
                        if fee_usd.is_some() || label.is_some() {
                            anyhow::bail!("--clear takes no fee or label")
                        }
                    } else if fee_usd.is_none() {
                        anyhow::bail!("provide a fee or --clear")
                    }
                    usage::billing::edit(&store, Utc::now(), |billing| {
                        let entry = billing.profiles.entry(profile.clone()).or_default();
                        if clear {
                            entry.plan = None;
                        } else {
                            usage::billing::set_plan(entry, fee_usd.unwrap(), label);
                        }
                        Ok(())
                    })?;
                    format!("Plan updated for {profile}.\n")
                }
                Some(UsageAction::Rate {
                    profile,
                    flat,
                    input,
                    output,
                    cache_write_5m,
                    cache_write_1h,
                    cache_read,
                    model_prefix,
                    clear,
                }) => {
                    require_billing_class(&manager, &profile, true, "rates")?;
                    let typed = [input, output, cache_write_5m, cache_write_1h, cache_read];
                    if clear {
                        if flat.is_some()
                            || typed.iter().any(Option::is_some)
                            || !model_prefix.is_empty()
                        {
                            anyhow::bail!("--clear takes no rates or prefixes")
                        }
                    } else if flat.is_none() && !typed.iter().all(Option::is_some) {
                        anyhow::bail!("provide --flat or all five per-type rates")
                    }
                    usage::billing::edit(&store, Utc::now(), |billing| {
                        let entry = billing.profiles.entry(profile.clone()).or_default();
                        entry.rate = if clear {
                            None
                        } else {
                            Some(usage::billing::Rate {
                                model_prefixes: model_prefix,
                                price: if let Some(flat) = flat {
                                    usage::billing::RatePrice::Flat { flat }
                                } else {
                                    usage::billing::RatePrice::PerType(usage::rates::Price {
                                        input: input.unwrap(),
                                        output: output.unwrap(),
                                        cache_write_5m: cache_write_5m.unwrap(),
                                        cache_write_1h: cache_write_1h.unwrap(),
                                        cache_read: cache_read.unwrap(),
                                    })
                                },
                            })
                        };
                        Ok(())
                    })?;
                    format!("Rate updated for {profile}.\n")
                }
                Some(UsageAction::Reset {
                    profile,
                    at,
                    list,
                    undo,
                }) => {
                    let local_now = Local::now();
                    reset_action(
                        &manager,
                        &store,
                        &profile,
                        if list {
                            ResetCommand::List
                        } else if undo {
                            ResetCommand::Undo
                        } else {
                            ResetCommand::Record(at.as_deref())
                        },
                        local_now.with_timezone(&Utc),
                        local_now.offset().fix(),
                    )?
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
        Some(Commands::Statusline { .. }) => unreachable!("handled before manager setup"),
    }

    Ok(())
}

#[cfg(test)]
mod statusline_flags_tests {
    use super::*;

    #[test]
    fn install_uninstall_targets_and_force_are_checked_by_clap() {
        // Known-bad: accepting a target without an action or --force on uninstall.
        for args in [
            vec!["cswitch", "statusline", "--install"],
            vec!["cswitch", "statusline", "--uninstall"],
            vec!["cswitch", "statusline", "work"],
            vec!["cswitch", "statusline", "--all"],
            vec!["cswitch", "statusline", "--force"],
            vec!["cswitch", "statusline", "--install", "work", "--all"],
            vec!["cswitch", "statusline", "--uninstall", "work", "--force"],
            vec!["cswitch", "statusline", "--install", "--uninstall", "work"],
        ] {
            assert!(Cli::try_parse_from(args.clone()).is_err(), "{args:?}");
        }
        for args in [
            vec!["cswitch", "statusline"],
            vec!["cswitch", "statusline", "--no-refresh"],
            vec![
                "cswitch",
                "statusline",
                "--install",
                "work",
                "--no-refresh",
                "--force",
            ],
            vec!["cswitch", "statusline", "--uninstall", "--all"],
        ] {
            assert!(Cli::try_parse_from(args.clone()).is_ok(), "{args:?}");
        }
    }
}

fn remove_profile_with_usage(
    manager: &ProfileManager,
    name: &str,
    purge_usage: bool,
    force: bool,
    directory: Option<std::path::PathBuf>,
) -> Result<()> {
    let profile = manager.get_profile(name)?;
    if profile.tool == Tool::Antigravity && !force {
        let local = manager
            .agy_farm_health(name)
            .map(|health| health.local)
            .unwrap_or_default();
        if !local.is_empty() {
            anyhow::bail!(
                "Profile '{name}' has local HOME entries: {}. Removing the profile deletes them. Run: cswitch remove {name} --force",
                agy::local_summary(&local, 10)
            );
        }
    }
    if purge_usage {
        let store = usage::store(manager, directory)?;
        usage::billing::purge(&store, name)?;
    }
    manager.remove_profile(name)
}

#[cfg(all(test, unix))]
mod agy_remove_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn setup(tool: Tool) -> (TempDir, ProfileManager) {
        let tmp = tempfile::tempdir().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join("real/.claude"))
                .unwrap();
        let mut registry = manager.load_registry().unwrap();
        registry.profiles.insert(
            "g".into(),
            profile::Profile {
                name: "g".into(),
                tool,
                email: None,
                added: Utc::now(),
                last_used: None,
            },
        );
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(manager.profile_dir("g").join("home/.gemini")).unwrap();
        (tmp, manager)
    }

    #[test]
    fn agy_remove_refuses_local_before_purging_usage_and_force_removes() {
        // Known-bad: no local-entry check, or a check after the usage purge.
        let (tmp, manager) = setup(Tool::Antigravity);
        let home = manager.profile_dir("g").join("home");
        fs::write(home.join("local-note"), b"keep exactly").unwrap();
        let usage = tmp.path().join("usage");
        fs::create_dir(&usage).unwrap();
        let usage_path = usage.join("billing.json");
        fs::write(&usage_path, b"{bad").unwrap();
        let registry_path = manager.base_dir.join("registry.json");
        let registry_before = fs::read(&registry_path).unwrap();
        let error = remove_profile_with_usage(&manager, "g", true, false, Some(usage))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("local-note") && error.contains("cswitch remove g --force"),
            "{error}"
        );
        assert_eq!(fs::read(&registry_path).unwrap(), registry_before);
        assert_eq!(fs::read(&usage_path).unwrap(), b"{bad");
        assert_eq!(fs::read(home.join("local-note")).unwrap(), b"keep exactly");
        assert!(manager.profile_dir("g").exists());

        let real_entry = tmp.path().join("real-entry");
        fs::write(&real_entry, b"real intact").unwrap();
        symlink(&real_entry, home.join("linked")).unwrap();
        remove_profile_with_usage(&manager, "g", false, true, None).unwrap();
        assert!(!manager.profile_dir("g").exists());
        assert!(manager.get_profile("g").is_err());
        assert_eq!(fs::read(real_entry).unwrap(), b"real intact");
    }

    #[test]
    fn agy_remove_accepts_links_and_gemini_without_force() {
        // Known-bad: .gemini or farm links counted as local entries.
        let (tmp, manager) = setup(Tool::Antigravity);
        let real_entry = tmp.path().join("real-entry");
        fs::write(&real_entry, b"real intact").unwrap();
        symlink(&real_entry, manager.profile_dir("g").join("home/linked")).unwrap();
        remove_profile_with_usage(&manager, "g", false, false, None).unwrap();
        assert_eq!(fs::read(real_entry).unwrap(), b"real intact");
        assert!(manager.get_profile("g").is_err());
    }

    #[test]
    fn agy_remove_accepts_missing_or_linked_home_without_force() {
        // Known-bad: propagating farm_health errors blocks removal of an unreadable farm.
        for linked in [false, true] {
            let (tmp, manager) = setup(Tool::Antigravity);
            let home = manager.profile_dir("g").join("home");
            fs::remove_dir_all(&home).unwrap();
            let outside = tmp.path().join("outside");
            if linked {
                fs::create_dir(&outside).unwrap();
                fs::write(outside.join("keep"), b"outside intact").unwrap();
                symlink(&outside, &home).unwrap();
            }
            remove_profile_with_usage(&manager, "g", false, false, None).unwrap();
            assert!(manager.get_profile("g").is_err());
            assert!(!manager.profile_dir("g").exists());
            if linked {
                assert_eq!(fs::read(outside.join("keep")).unwrap(), b"outside intact");
            }
        }
    }

    #[test]
    fn remove_force_is_optional_for_existing_tools() {
        // Known-bad: applying the Antigravity local-entry guard to Claude or Codex.
        for tool in [Tool::Claude, Tool::Codex] {
            let (_tmp, manager) = setup(tool);
            fs::write(
                manager.profile_dir("g").join("home/local-note"),
                b"only here",
            )
            .unwrap();
            remove_profile_with_usage(&manager, "g", false, false, None).unwrap();
            assert!(manager.get_profile("g").is_err());
        }
    }

    #[test]
    fn local_summary_is_sorted_and_bounded() {
        // Known-bad: raw unsorted names or an unbounded refusal message.
        let entries = (0..12)
            .rev()
            .map(|n| format!("entry-{n:02}"))
            .collect::<Vec<_>>();
        let summary = agy::local_summary(&entries, 10);
        assert!(summary.starts_with("entry-00, entry-01"));
        assert!(summary.ends_with("entry-09, and 2 more"));
    }

    #[test]
    fn agy_remove_refusal_wires_the_ten_entry_limit() {
        // Known-bad: the CLI passes a limit other than ten to local_summary.
        let (_tmp, manager) = setup(Tool::Antigravity);
        let home = manager.profile_dir("g").join("home");
        for index in 0..12 {
            fs::write(home.join(format!("local-{index:02}")), b"local").unwrap();
        }
        let message = remove_profile_with_usage(&manager, "g", false, false, None)
            .unwrap_err()
            .to_string();
        assert_eq!(message.matches("local-").count(), 10, "{message}");
        assert!(message.contains("local-00, local-01"), "{message}");
        assert!(
            message.contains("local-09, and 2 more. Removing"),
            "{message}"
        );
        assert!(!message.contains("local-10") && !message.contains("local-11"));
        assert!(manager.profile_dir("g").exists());
    }

    #[test]
    fn local_control_characters_are_sanitized_in_remove_and_info() {
        // Known-bad: newline, tab or escape in a local filename reaches terminal output.
        let (_tmp, manager) = setup(Tool::Antigravity);
        let name = "line\nwith\ttab\x1besc";
        fs::write(manager.profile_dir("g").join("home").join(name), b"local").unwrap();
        let refusal = remove_profile_with_usage(&manager, "g", false, false, None)
            .unwrap_err()
            .to_string();
        let info = info_output(&manager, "g", Utc::now()).unwrap();
        for output in [refusal, info] {
            assert!(output.contains("line with tab esc"), "{output:?}");
            assert!(!output.contains("line\nwith"), "{output:?}");
            assert!(
                !output.contains('\t') && !output.contains('\x1b'),
                "{output:?}"
            );
        }
    }
}

fn require_billing_class(
    manager: &ProfileManager,
    name: &str,
    per_token: bool,
    subject: &str,
) -> Result<()> {
    let profile = manager.get_profile(name)?;
    if profile.tool != Tool::Claude {
        anyhow::bail!("{name} is not a Claude profile")
    }
    let mode = key::read_auth_mode(manager, name, read_claude_json(&manager.profile_dir(name)));
    if per_token && !mode.api_billed() {
        anyhow::bail!("{name} is not a per-token profile; rates apply to per-token profiles")
    }
    if !per_token && mode != key::AuthMode::Subscription {
        anyhow::bail!(
            "{name} is not a subscription profile; {subject} apply to subscription profiles"
        )
    }
    Ok(())
}

enum ResetCommand<'a> {
    Record(Option<&'a str>),
    List,
    Undo,
}

fn reset_action(
    manager: &ProfileManager,
    store: &usage::ledger::Store,
    profile: &str,
    command: ResetCommand<'_>,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> Result<String> {
    require_billing_class(manager, profile, false, "limit resets")?;
    if matches!(command, ResetCommand::List) {
        let settings = usage::billing::read(&store.dir, now)?;
        let Some(entry) = settings.profiles.get(profile) else {
            return Ok(format!("No resets recorded for {profile}.\n"));
        };
        if entry.limit_resets.is_empty() {
            return Ok(format!("No resets recorded for {profile}.\n"));
        }
        return Ok(entry
            .limit_resets
            .iter()
            .enumerate()
            .map(|(index, reset)| {
                let from = reset.from.with_timezone(&offset).format("%Y-%m-%d %H:%M");
                let to = reset.to.with_timezone(&offset).format("%Y-%m-%d %H:%M");
                format!("{}. {from} to {to}\n", index + 1)
            })
            .collect());
    }
    if matches!(command, ResetCommand::Undo) {
        let mut removed = false;
        usage::billing::edit(store, now, |billing| {
            let entry = billing.profiles.entry(profile.into()).or_default();
            removed = entry.limit_resets.pop().is_some();
            Ok(())
        })?;
        return Ok(if removed {
            format!("Newest reset removed for {profile}.\n")
        } else {
            format!("No resets recorded for {profile}.\n")
        });
    }
    let ResetCommand::Record(at) = command else {
        unreachable!()
    };
    let bracket = usage::billing::parse_reset_bracket(at, now, offset)?;
    usage::billing::edit(store, now, |billing| {
        let entry = billing.profiles.entry(profile.into()).or_default();
        entry.limit_resets.push(bracket);
        entry.limit_resets.sort_by_key(|reset| reset.from);
        Ok(())
    })?;
    Ok(format!("Reset recorded for {profile}.\n"))
}

fn info_output(manager: &ProfileManager, name: &str, now: DateTime<Utc>) -> Result<String> {
    let profile = manager.get_profile(name)?;
    let dir = manager.profile_dir(&profile.name);
    let mut output = format!(
        "Name:      {}\nTool:      {}\nEmail:     {}\n",
        profile.name,
        profile.tool.label(),
        profile.email.as_deref().unwrap_or("—")
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
        let usage_dir = usage_directory(manager);
        output.push_str(&usage_info(
            &usage_dir,
            &profile.name,
            &auth,
            now,
            Local::now().offset().fix(),
            &limits,
        ));
    } else {
        output.push_str("Auth:      —\nGateway:   —\n");
        if profile.tool == Tool::Codex {
            let plan = manager
                .codex_identity(&profile.name)
                .and_then(|identity| identity.plan_type)
                .unwrap_or_else(|| "—".into());
            output.push_str(&format!("Plan:      {plan}\n"));
        } else if profile.tool == Tool::Antigravity {
            output.push_str(&format!(
                "Home:      {}\n",
                agy::profile_home(&dir).display()
            ));
            match manager.agy_farm_health(name) {
                Ok(health) => {
                    output.push_str(&format!(
                        "Farm:      {} {}, {} dangling\nLocal:     {}\n",
                        health.links,
                        if health.links == 1 { "link" } else { "links" },
                        health.dangling,
                        if health.local.is_empty() {
                            "—".into()
                        } else {
                            agy::local_summary(&health.local, health.local.len())
                        }
                    ));
                }
                Err(_) => output.push_str("Farm:      unavailable\nLocal:     —\n"),
            }
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

fn usage_directory(manager: &ProfileManager) -> std::path::PathBuf {
    std::env::var_os("CSWITCH_USAGE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| manager.base_dir.join("usage"))
}

fn usage_info(
    dir: &Path,
    name: &str,
    auth: &key::AuthMode,
    now: DateTime<Utc>,
    offset: FixedOffset,
    limits: &Limits,
) -> String {
    use usage::metrics as m;
    let hourly = m::read(dir);
    if hourly.is_none() {
        return "Usage:     no ledger yet — run `cswitch usage`\n".into();
    }
    let settings = m::load_settings(dir, now);
    let history = m::history(dir);
    usage_info_cached(
        UsageInfoData {
            hourly: hourly.as_ref(),
            settings: &settings,
            history: &history,
        },
        name,
        auth,
        now,
        offset,
        limits,
    )
}

type UsageSettings =
    std::result::Result<(usage::rates::Rates, usage::billing::Billing), (&'static str, String)>;

struct UsageInfoData<'a> {
    hourly: Option<&'a usage::metrics::Hourly>,
    settings: &'a UsageSettings,
    history: &'a [usage::metrics::LimitRow],
}

fn usage_info_cached(
    data: UsageInfoData<'_>,
    name: &str,
    auth: &key::AuthMode,
    now: DateTime<Utc>,
    offset: FixedOffset,
    limits: &Limits,
) -> String {
    use usage::metrics as m;
    let Some(hourly) = data.hourly else {
        return "Usage:     no ledger yet — run `cswitch usage`\n".into();
    };
    let (rates, billing) = match data.settings {
        Ok(settings) => settings,
        Err((file, reason)) => {
            return format!("Usage:     usage settings unreadable: {file} — {reason}\n");
        }
    };
    let per_token = auth.api_billed();
    let entry = billing.profiles.get(name);
    let sums = m::windows(&hourly.rows, name, now, offset, rates, entry, per_token);
    let age = profile::describe_age((now - hourly.generated_at).num_seconds().max(0) as u64);
    let mut out = format!("Usage (ledger as of {age}):\n");
    for (label, value) in ["Today", "7 days", "30 days"].iter().zip(sums.iter()) {
        let money = if per_token {
            format!(
                "{} spent · {} list price",
                value.spend_cell(),
                value.list_info_cell()
            )
        } else {
            format!("{} list price", value.list_info_cell())
        };
        out.push_str(&format!(
            "  {label:<9} {:>8} tok   {money}\n",
            m::tokens(value.tokens)
        ));
    }
    if now - hourly.generated_at > chrono::Duration::hours(24) {
        out.push_str("  usage figures are over 24 h old — run cswitch usage to refresh\n");
    }
    if per_token {
        if let Some(rate) = entry.and_then(|e| e.rate.as_ref()) {
            let prices = match &rate.price {
                usage::billing::RatePrice::Flat { flat } => {
                    format!("{} per 1M tokens, flat", m::rate_money(*flat))
                }
                usage::billing::RatePrice::PerType(p) => format!(
                    "per 1M tokens: input {}, output {}, 5m write {}, 1h write {}, read {}",
                    m::rate_money(p.input),
                    m::rate_money(p.output),
                    m::rate_money(p.cache_write_5m),
                    m::rate_money(p.cache_write_1h),
                    m::rate_money(p.cache_read)
                ),
            };
            let scope = if rate.model_prefixes.is_empty() {
                "all models".into()
            } else {
                format!(
                    "models {}*; other models at list price",
                    rate.model_prefixes.join("*, ")
                )
            };
            out.push_str(&format!("Rate:      {prices} ({scope})\n"));
        } else {
            out.push_str(&format!(
                "Rate:      not set — cswitch usage rate {name} --flat <usd>\n"
            ));
        }
        if sums[2].tokens > 0 && sums[2].spend_priced > 0 {
            let rate = sums[2].spend * 1_000_000.0 / sums[2].tokens as f64;
            let star = if sums[2].spend_unpriced > 0 { "*" } else { "" };
            out.push_str(&format!(
                "Effective: {}{star} per 1M tokens over 30 days (spend ÷ tokens)\n",
                m::rate_money(rate)
            ));
        }
    } else if *auth == key::AuthMode::Subscription {
        if let Some(plan) = entry.and_then(|e| e.plan.as_ref()) {
            let value = sums[2].value;
            let ratio = if plan.fee_usd > 0.0 {
                format!(" ({:.0}× the fee)", value / plan.fee_usd)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "Plan:      {} · {}/mo · last 30 days {} of list-price usage{ratio}\n",
                plan.label,
                m::money(plan.fee_usd),
                m::money(value)
            ));
            if sums[2].tokens > 0 {
                out.push_str(&format!(
                    "Effective: {} per 1M tokens over 30 days (fee ÷ tokens)\n",
                    m::rate_money(plan.fee_usd * 1_000_000.0 / sums[2].tokens as f64)
                ));
            }
        } else {
            out.push_str(&format!(
                "Plan:      not set — cswitch usage plan {name} <fee> --label <text>\n"
            ));
        }
        let mut history = data.history.to_vec();
        if let Limits::Snapshot(snapshot) = limits
            && let Some(weekly) = snapshot
                .weekly()
                .filter(|w| w.kind == "weekly_all")
                .and_then(|w| {
                    w.resets_at.map(|r| m::Weekly {
                        percent: w.percent,
                        resets_at: r,
                        window_started_at: snapshot.window_started_at,
                    })
                })
        {
            history.push(m::LimitRow {
                profile: name.into(),
                fetched_at: snapshot.fetched_at,
                weekly,
            });
        }
        let report = m::capacity(
            name,
            &history,
            hourly,
            rates,
            entry.map_or(&[][..], |entry| entry.limit_resets.as_slice()),
        );
        if let Some(c) = report.estimate {
            let star = if c.partial { "*" } else { "" };
            let lower = m::capacity_money(c.lower);
            let upper = m::capacity_money(c.upper);
            let amount = if c.after_reset.is_some() && lower != upper {
                format!("{lower}–{upper}")
            } else {
                lower
            };
            let reset = c
                .after_reset
                .map(|at| {
                    format!(
                        ", after the reset on {}",
                        at.with_timezone(&offset).format("%m-%d")
                    )
                })
                .unwrap_or_default();
            out.push_str(&format!("Capacity:  weekly limit ≈ {amount}{star} of list-price usage (est. from {:.0}% at {}{reset}; last 4 estimates {}–{})\n",
                c.snapshot.weekly.percent,c.snapshot.fetched_at.with_timezone(&offset).format("%m-%d %H:%M"),
                m::capacity_money(c.min),m::capacity_money(c.max)));
        } else {
            out.push_str(&format!(
                "Capacity:  not enough data yet ({})\n",
                report.reason
            ));
        }
        if let Some((from, to)) = report.hint {
            out.push_str(&format!("Reset:     detected between {} and {} — record it with cswitch usage reset {name} --at <YYYY-MM-DD>\n",
                from.with_timezone(&offset).format("%m-%d %H:%M"),
                to.with_timezone(&offset).format("%m-%d %H:%M")));
        }
    }
    out
}

fn usage_30d_cell(
    name: &str,
    auth: &key::AuthMode,
    hourly: Option<&usage::metrics::Hourly>,
    settings: &UsageSettings,
    now: DateTime<Utc>,
    offset: FixedOffset,
) -> String {
    let (Some(hourly), Ok((rates, billing))) = (hourly, settings) else {
        return "—".into();
    };
    let values = usage::metrics::windows(
        &hourly.rows,
        name,
        now,
        offset,
        rates,
        billing.profiles.get(name),
        auth.api_billed(),
    );
    if auth.api_billed() {
        values[2].spend_cell()
    } else {
        values[2].list_cell()
    }
}

fn list_output(manager: &ProfileManager, now: DateTime<Utc>) -> Result<String> {
    use usage::metrics as m;
    let profiles = manager.list_profiles()?;
    if profiles.is_empty() {
        return Ok("No profiles found. Add one with:\n  cswitch add <name>\n".into());
    }
    let header = format!(
        "{:<20} {:<11} {:<26} {:<7} {:<18} {:<11} {:<9} {:<11}",
        "NAME", "TOOL", "EMAIL", "5H", "7D", "AS OF", "30D $", "LAST USED"
    );
    let mut out = format!("{header}\n{}\n", "─".repeat(header.chars().count()));
    let dir = usage_directory(manager);
    let hourly = m::read(&dir);
    let settings = m::load_settings(&dir, now);
    let mut saw_reset = false;
    let mut saw_flag = false;
    let mut saw_api = false;
    let mut saw_usage = false;
    let mut saw_claude = false;
    for p in profiles {
        let name: String = p.name.chars().take(20).collect();
        let email: String = p.email.as_deref().unwrap_or("—").chars().take(26).collect();
        let tool: String = if matches!(p.tool, Tool::Unknown(_)) {
            "unknown".into()
        } else {
            p.tool.label().chars().take(11).collect()
        };
        let last = p
            .last_used
            .map(|t| t.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".into());
        let (session, weekly, age, cell) = if p.tool != Tool::Claude {
            ("—".into(), "—".into(), "—".into(), "—".into())
        } else {
            saw_claude = true;
            let dir = manager.profile_dir(&p.name);
            let claude = read_claude_json(&dir);
            let auth = key::read_auth_mode(manager, &p.name, claude.clone());
            let limits = claude
                .map(|j| j.map_or(Limits::Unreadable, |v| parse_limits(&v)))
                .unwrap_or(Limits::Unreadable);
            let (session, weekly, age) = if auth.api_billed() {
                saw_api = true;
                ("—".into(), "—".into(), "api".into())
            } else {
                match limits {
                    Limits::Snapshot(snapshot) => {
                        let a = list_window(snapshot.session(), now, false);
                        let b = list_window(snapshot.weekly(), now, true);
                        saw_reset |= a.1 || b.1;
                        saw_flag |= a.2 || b.2;
                        (a.0, b.0, snapshot.age(now))
                    }
                    Limits::NoSnapshot => ("—".into(), "—".into(), "no data".into()),
                    Limits::AccountMismatch => ("—".into(), "—".into(), "mismatch".into()),
                    Limits::Unreadable => ("—".into(), "—".into(), "unreadable".into()),
                }
            };
            let cell = usage_30d_cell(
                &p.name,
                &auth,
                hourly.as_ref(),
                &settings,
                now,
                Local::now().offset().fix(),
            );
            saw_usage |= cell != "—";
            (session, weekly, age, cell)
        };
        out.push_str(&format!(
            "{:<20} {:<11} {:<26} {:<7} {:<18} {:<11} {:<9} {:<11}\n",
            name,
            tool,
            email,
            session,
            weekly,
            age,
            cell.chars().take(9).collect::<String>(),
            last
        ));
    }
    if saw_api {
        out.push_str("\napi = billed per token, no plan limits\n")
    }
    if saw_reset || saw_flag {
        let mut parts = Vec::new();
        if saw_reset {
            parts.push("reset = that window restarted after the snapshot was taken")
        }
        if saw_flag {
            parts.push("! = Claude Code flags this limit")
        }
        out.push_str(&format!("\n{}\n", parts.join(" · ")));
    }
    if saw_usage {
        out.push_str("\n30D $ = last 30 days: spend for per-token profiles · ~ = list-price value of a plan's usage\n")
    }
    if let Err((file, reason)) = &settings {
        out.push_str(&format!("\nusage settings unreadable: {file} — {reason}\n"));
    }
    if saw_claude && hourly.is_none() {
        out.push_str("\nno ledger yet — run `cswitch usage`\n");
    }
    if let Some(hourly) = hourly
        && now - hourly.generated_at > chrono::Duration::hours(24)
    {
        out.push_str(&format!(
            "usage figures as of {} — run cswitch usage to refresh\n",
            profile::describe_age((now - hourly.generated_at).num_seconds().max(0) as u64)
        ));
    }
    Ok(out)
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
        } else if outcome.tool == Tool::Antigravity {
            println!(
                "  To use a different Google account, choose it in Antigravity's sign-in flow"
            );
            println!("  after removing this profile: cswitch remove {name}");
        }
    }

    println!("\nLaunch with: cswitch use {}", name);
}

fn login_confirmation(name: &str, outcome: &LoginOutcome) -> String {
    if outcome.email.is_none() && outcome.tool != Tool::Claude {
        let label = if outcome.tool == Tool::Codex {
            "Codex"
        } else {
            "Antigravity"
        };
        format!("{label} login completed for profile '{name}' (email unavailable).")
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
        if io::stdin().read_line(&mut input)? == 0 {
            anyhow::bail!("No answer on standard input. Nothing was changed.");
        }

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
            "{:<20} {:<11} {:<26} {:<7} {:<18} {:<11} {:<9} {:<11}",
            "NAME", "TOOL", "EMAIL", "5H", "7D", "AS OF", "30D $", "LAST USED"
        );
        let row = |name: &str, email: &str, five: &str, seven: &str, age: &str, last: &str| {
            format!(
                "{:<20} {:<11} {:<26} {:<7} {:<18} {:<11} {:<9} {:<11}\n",
                name,
                "claude",
                email.chars().take(26).collect::<String>(),
                five,
                seven,
                age,
                "—",
                last
            )
        };
        let expected = format!(
            "{header}\n{}\n{}{}",
            "─".repeat(120),
            row(
                "active",
                "thirtyx.characters@example.com",
                "12%",
                "88% █████████░",
                "1 h ago",
                "01-07 06:31"
            ),
            row("fresh", "b@example.com", "—", "—", "no data", "never")
        );
        let output = list_output(&manager, now).unwrap();
        assert_eq!(
            output,
            format!("{expected}\nno ledger yet — run `cswitch usage`\n")
        );
        assert!(output.lines().all(|line| line.chars().count() <= 120));

        cache["cachedUsageUtilization"]["utilization"]["limits"][0]["resets_at"] =
            serde_json::json!("2030-01-07T12:00:00Z");
        cache["cachedUsageUtilization"]["utilization"]["limits"][1]["severity"] =
            serde_json::json!("critical");
        fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
        let changed = list_output(&manager, now).unwrap();
        let expected_changed = format!(
            "{header}\n{}\n{}{}\nreset = that window restarted after the snapshot was taken · ! = Claude Code flags this limit\n",
            "─".repeat(120),
            row(
                "active",
                "thirtyx.characters@example.com",
                "reset",
                "88% █████████░ !",
                "1 h ago",
                "01-07 06:31"
            ),
            row("fresh", "b@example.com", "—", "—", "no data", "never")
        );
        assert_eq!(
            changed,
            format!("{expected_changed}\nno ledger yet — run `cswitch usage`\n")
        );
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

    #[test]
    fn antigravity_add_and_login_flags_parse() {
        // Known-bad: --tool agy is accepted by clap but dispatched as Claude.
        for value in ["antigravity", "agy"] {
            assert!(matches!(
                Cli::try_parse_from(["cswitch", "add", "g", "--tool", value])
                    .unwrap()
                    .command,
                Some(Commands::Add {
                    tool: ToolChoice::Antigravity,
                    ..
                })
            ));
            assert!(matches!(
                Cli::try_parse_from(["cswitch", "login", "g", "--tool", value])
                    .unwrap()
                    .command,
                Some(Commands::Login {
                    tool: ToolChoice::Antigravity,
                    ..
                })
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_info_shows_home_and_farm_health() {
        // Known-bad: info has only the profile directory and hides the fake HOME's local entries.
        let temp = TempDir::new().unwrap();
        let manager = ProfileManager::with_base_dir(temp.path().join(".claude-switch")).unwrap();
        fs::write(temp.path().join("shared"), b"synthetic").unwrap();
        let profile_dir = manager.profile_dir("g");
        fs::create_dir_all(&profile_dir).unwrap();
        agy::link_farm(temp.path(), &profile_dir).unwrap();
        fs::write(
            profile_dir.join(".claude.json"),
            br#"{"primaryApiKey":"synthetic"}"#,
        )
        .unwrap();
        fs::write(agy::profile_home(&profile_dir).join("local"), b"local").unwrap();
        let registry = Registry {
            profiles: [(
                "g".into(),
                Profile {
                    name: "g".into(),
                    tool: Tool::Antigravity,
                    email: None,
                    added: Utc::now(),
                    last_used: None,
                },
            )]
            .into(),
        };
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        let output = info_output(&manager, "g", Utc::now()).unwrap();
        assert!(output.contains("Tool:      antigravity"), "{output}");
        assert!(output.contains("Email:     —"), "{output}");
        assert!(output.contains("Auth:      —"), "{output}");
        assert!(!output.contains("Console (API billing)"), "{output}");
        assert!(
            output.contains(&format!(
                "Home:      {}",
                agy::profile_home(&profile_dir).display()
            )),
            "{output}"
        );
        assert!(output.contains("Farm:      1 link, 0 dangling"), "{output}");
        fs::write(temp.path().join("second-shared"), b"synthetic").unwrap();
        agy::link_farm(temp.path(), &profile_dir).unwrap();
        let two_links = info_output(&manager, "g", Utc::now()).unwrap();
        assert!(
            two_links.contains("Farm:      2 links, 0 dangling"),
            "{two_links}"
        );
        assert!(output.contains("Local:     local"), "{output}");
        // Known-bad: reusing remove's ten-entry limit truncates info's Local line.
        let mut expected_local = vec!["local".to_string()];
        for n in 1..=11 {
            let name = format!("local-{n:02}");
            fs::write(agy::profile_home(&profile_dir).join(&name), b"synthetic").unwrap();
            expected_local.push(name);
        }
        let all_local = info_output(&manager, "g", Utc::now()).unwrap();
        let local_line = all_local
            .lines()
            .find_map(|line| line.strip_prefix("Local:     "))
            .unwrap();
        assert_eq!(local_line.split(", ").collect::<Vec<_>>(), expected_local);
        assert!(!local_line.contains("more"), "{all_local}");
        assert_eq!(
            require_billing_class(&manager, "g", true, "rates")
                .unwrap_err()
                .to_string(),
            "g is not a Claude profile"
        );
    }

    #[test]
    fn antigravity_list_row_ignores_claude_limit_cache() {
        // Known-bad: only Codex skips the Claude list path.
        let temp = TempDir::new().unwrap();
        let manager = ProfileManager::with_base_dir(temp.path().join(".claude-switch")).unwrap();
        let mut registry = Registry::default();
        registry.profiles.insert(
            "g".into(),
            Profile {
                name: "g".into(),
                tool: Tool::Antigravity,
                email: None,
                added: Utc::now(),
                last_used: None,
            },
        );
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(manager.profile_dir("g")).unwrap();
        let now = DateTime::parse_from_rfc3339("2030-01-07T13:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let cache = serde_json::json!({
            "oauthAccount": {"accountUuid":"00000000-0000-4000-8000-000000000001"},
            "cachedUsageUtilization": {
                "accountUuid":"00000000-0000-4000-8000-000000000001",
                "fetchedAtMs":now.timestamp_millis(),
                "utilization":{"limits":[
                    {"kind":"session","group":"session","percent":12,"severity":"normal","resets_at":"2030-01-07T15:00:00Z"},
                    {"kind":"weekly_all","group":"weekly","percent":88,"severity":"normal","resets_at":"2030-01-10T20:00:00Z"}
                ]}
            }
        });
        fs::write(
            manager.profile_dir("g").join(".claude.json"),
            cache.to_string(),
        )
        .unwrap();
        let output = list_output(&manager, now).unwrap();
        assert!(output.lines().all(|line| line.chars().count() <= 120));
        let row = output.lines().find(|line| line.starts_with("g ")).unwrap();
        let cells: Vec<_> = row.split_whitespace().collect();
        assert_eq!(cells[1], "antigravity");
        assert_eq!(&cells[3..7], &["—", "—", "—", "—"]);
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
        assert!(output.contains("u                    unknown"), "{output}");
        assert!(
            !output.lines().any(|line| line.contains("unknown too")),
            "{output}"
        ); // Known-bad: truncated unknown tool label.
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
            "Name:      c\nTool:      claude\nEmail:     {}\nAuth:      not logged in\nGateway:   the Anthropic API\nPlan limits: the file couldn't be read.\nUsage:     no ledger yet — run `cswitch usage`\nAdded:     {}\nLast used: never\nDirectory: {}\n\nLaunch:\n  cswitch use c\n",
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

#[cfg(test)]
mod billing_view_tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    use tempfile::TempDir;
    fn usage_tree(
        root: &Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, (Option<Vec<u8>>, std::time::SystemTime)>
    {
        fn walk(
            root: &Path,
            dir: &Path,
            out: &mut std::collections::BTreeMap<
                std::path::PathBuf,
                (Option<Vec<u8>>, std::time::SystemTime),
            >,
        ) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::metadata(&path).unwrap();
                let bytes = if meta.is_file() {
                    Some(fs::read(&path).unwrap())
                } else {
                    None
                };
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    (bytes, meta.modified().unwrap()),
                );
                if meta.is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out);
        out
    }
    #[test]
    fn plan_views_union_live_snapshot_without_writing_any_usage_file() {
        // Known-bads: omitting the live snapshot or its window_started_at, or writing limits.jsonl on a read path.
        let tmp = TempDir::new().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = Utc.with_ymd_and_hms(2030, 1, 7, 12, 0, 0).unwrap();
        let mut registry = profile::Registry::default();
        registry.profiles.insert(
            "p".into(),
            profile::Profile {
                name: "p".into(),
                tool: Tool::Claude,
                email: Some("p@example.com".into()),
                added: now,
                last_used: None,
            },
        );
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(manager.profile_dir("p")).unwrap();
        let live = serde_json::json!({"oauthAccount":{},"cachedUsageUtilization":{
            "fetchedAtMs":Utc.with_ymd_and_hms(2030,1,5,0,0,0).unwrap().timestamp_millis(),
            "utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":50,
                "resets_at":"2030-01-08T00:00:00Z"}],
                "seven_day_breakdown":{"window_started_at":"2030-01-02T00:00:00Z"}}}});
        fs::write(
            manager.profile_dir("p").join(".claude.json"),
            live.to_string(),
        )
        .unwrap();
        let usage = manager.base_dir.join("usage");
        fs::create_dir_all(&usage).unwrap();
        let hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: Utc.with_ymd_and_hms(2030, 1, 6, 0, 0, 0).unwrap(),
            rows: vec![
                usage::metrics::Bucket {
                    profile: "p".into(),
                    hour: Utc.with_ymd_and_hms(2030, 1, 1, 23, 0, 0).unwrap(),
                    model: "claude-opus-5".into(),
                    speed: None,
                    requests: 1,
                    input: 1_000_000,
                    output: 0,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    cache_read: 0,
                },
                usage::metrics::Bucket {
                    profile: "p".into(),
                    hour: Utc.with_ymd_and_hms(2030, 1, 2, 0, 0, 0).unwrap(),
                    model: "claude-opus-5".into(),
                    speed: None,
                    requests: 1,
                    input: 1_000_000,
                    output: 0,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    cache_read: 0,
                },
            ],
        };
        fs::write(
            usage.join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        fs::write(
            usage.join("billing.json"),
            r#"{"version":1,"profiles":{"p":{"plan":{"label":"Pro","fee_usd":20.0}}}}"#,
        )
        .unwrap();
        let without_history = info_output(&manager, "p", now).unwrap();
        assert!(
            without_history.contains("Capacity:  weekly limit ≈ $10 of list-price usage"),
            "{without_history}"
        );
        assert!(!usage.join("limits.jsonl").exists());
        fs::write(
            usage.join("limits.jsonl"),
            serde_json::json!({"profile":"p",
            "fetched_at":"2030-01-03T00:00:00Z","weekly":{"percent":40,
                "resets_at":"2030-01-08T00:00:00Z"}})
            .to_string()
                + "\n",
        )
        .unwrap();
        let before = usage_tree(&usage);
        let list = list_output(&manager, now).unwrap();
        let info = info_output(&manager, "p", now).unwrap();
        assert!(list.contains("30D $"));
        assert!(
            info.contains("Capacity:  weekly limit ≈ $10 of list-price usage"),
            "{info}"
        );
        assert_eq!(usage_tree(&usage), before);
    }
    #[test]
    fn read_only_views_preserve_usage_files_and_show_age() {
        // Known-bad: a list/info read seeds rates, takes the lock, or rewrites the rollup.
        let tmp = TempDir::new().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = Utc.with_ymd_and_hms(2030, 1, 8, 12, 0, 0).unwrap();
        let mut registry = profile::Registry::default();
        registry.profiles.insert(
            "p".into(),
            profile::Profile {
                name: "p".into(),
                tool: Tool::Claude,
                email: Some("p@example.com".into()),
                added: now,
                last_used: None,
            },
        );
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(manager.profile_dir("p")).unwrap();
        fs::write(
            manager.profile_dir("p").join(".claude.json"),
            r#"{"primaryApiKey":"synthetic"}"#,
        )
        .unwrap();
        let usage = manager.base_dir.join("usage");
        fs::create_dir_all(&usage).unwrap();
        let hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: now - chrono::Duration::hours(25),
            rows: vec![usage::metrics::Bucket {
                profile: "p".into(),
                hour: now - chrono::Duration::hours(1),
                model: "claude-opus-5".into(),
                speed: None,
                requests: 1,
                input: 1_000_000,
                output: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
                cache_read: 0,
            }],
        };
        let path = usage.join("hourly.json");
        let bytes = serde_json::to_vec(&hourly).unwrap();
        fs::write(&path, &bytes).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let list = list_output(&manager, now).unwrap();
        assert!(list.contains("$5.00"), "{list}");
        assert!(list.contains("usage figures as of"));
        assert!(list.lines().all(|line| line.chars().count() <= 120));
        let info = info_output(&manager, "p", now).unwrap();
        assert!(info.contains("Usage (ledger as of"), "{info}");
        assert!(info.contains("usage figures are over 24 h old"));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert!(!usage.join("rates.json").exists());
        assert!(!usage.join("billing.json").exists());
        assert!(!usage.join(".lock").exists());
    }
    #[test]
    fn malformed_settings_name_the_file_in_list_and_info() {
        // Known-bad: list silently showing dashes, and info blaming billing.json for broken rates.json.
        let tmp = TempDir::new().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = Utc.with_ymd_and_hms(2030, 1, 8, 12, 0, 0).unwrap();
        let mut registry = profile::Registry::default();
        registry.profiles.insert(
            "p".into(),
            profile::Profile {
                name: "p".into(),
                tool: Tool::Claude,
                email: Some("p@example.com".into()),
                added: now,
                last_used: None,
            },
        );
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(manager.profile_dir("p")).unwrap();
        fs::write(
            manager.profile_dir("p").join(".claude.json"),
            r#"{"primaryApiKey":"synthetic"}"#,
        )
        .unwrap();
        let usage = manager.base_dir.join("usage");
        fs::create_dir_all(&usage).unwrap();
        fs::write(
            usage.join("hourly.json"),
            serde_json::to_vec(&usage::metrics::Hourly {
                version: 1,
                generated_at: now,
                rows: vec![],
            })
            .unwrap(),
        )
        .unwrap();
        for file in ["billing.json", "rates.json"] {
            fs::write(usage.join(file), b"{bad").unwrap();
            let list = list_output(&manager, now).unwrap();
            let info = info_output(&manager, "p", now).unwrap();
            assert!(
                list.contains(&format!("usage settings unreadable: {file} —")),
                "{list}"
            );
            assert!(
                info.contains(&format!("usage settings unreadable: {file} —")),
                "{info}"
            );
            fs::remove_file(usage.join(file)).unwrap();
        }
    }
}

#[cfg(test)]
mod billing_cli_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    #[test]
    fn class_guards_refuse_every_wrong_profile_type() {
        // Known-bad: removing a plan, rate, reset, missing-auth or Codex class guard.
        let tmp = tempfile::tempdir().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let mut registry = profile::Registry::default();
        for (name, tool, auth) in [
            ("plan", Tool::Claude, r#"{"oauthAccount":{}}"#),
            ("keyed", Tool::Claude, r#"{"primaryApiKey":"synthetic"}"#),
            ("empty", Tool::Claude, "{}"),
            ("codex", Tool::Codex, r#"{"oauthAccount":{}}"#),
            ("codex_key", Tool::Codex, r#"{"primaryApiKey":"synthetic"}"#),
        ] {
            registry.profiles.insert(
                name.into(),
                profile::Profile {
                    name: name.into(),
                    tool,
                    email: Some(format!("{name}@example.com")),
                    added: Utc::now(),
                    last_used: None,
                },
            );
            std::fs::create_dir_all(manager.profile_dir(name)).unwrap();
            std::fs::write(manager.profile_dir(name).join(".claude.json"), auth).unwrap();
        }
        std::fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        assert!(require_billing_class(&manager, "plan", false, "plans").is_ok());
        assert!(require_billing_class(&manager, "keyed", true, "rates").is_ok());
        assert!(require_billing_class(&manager, "plan", true, "rates").is_err());
        assert_eq!(
            require_billing_class(&manager, "keyed", false, "plans")
                .unwrap_err()
                .to_string(),
            "keyed is not a subscription profile; plans apply to subscription profiles"
        );
        for name in ["empty", "codex", "codex_key"] {
            assert!(require_billing_class(&manager, name, true, "rates").is_err());
            assert!(require_billing_class(&manager, name, false, "plans").is_err());
        }
        for name in ["codex", "codex_key"] {
            assert_eq!(
                require_billing_class(&manager, name, false, "plans")
                    .unwrap_err()
                    .to_string(),
                format!("{name} is not a Claude profile")
            );
        }
        let store = usage::store(&manager, None).unwrap();
        assert_eq!(
            reset_action(
                &manager,
                &store,
                "keyed",
                ResetCommand::Record(None),
                Utc::now(),
                FixedOffset::east_opt(0).unwrap()
            )
            .unwrap_err()
            .to_string(),
            "keyed is not a subscription profile; limit resets apply to subscription profiles"
        );
    }
    #[test]
    fn reset_cli_uses_local_brackets_and_private_locked_settings() {
        // Known-bads: treating a local date as a UTC day, allowing a per-token reset,
        // writing billing.json world-readable, or waiting for a busy usage lock.
        let tmp = tempfile::tempdir().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = DateTime::parse_from_rfc3339("2030-01-11T12:00:00Z")
            .unwrap()
            .to_utc();
        let offset = FixedOffset::west_opt(3 * 3600).unwrap();
        let mut registry = profile::Registry::default();
        for (name, auth) in [
            ("p", r#"{"oauthAccount":{}}"#),
            ("q", r#"{"oauthAccount":{}}"#),
            ("k", r#"{"primaryApiKey":"synthetic"}"#),
        ] {
            registry.profiles.insert(
                name.into(),
                profile::Profile {
                    name: name.into(),
                    tool: Tool::Claude,
                    email: Some(format!("{name}@example.com")),
                    added: now,
                    last_used: None,
                },
            );
            std::fs::create_dir_all(manager.profile_dir(name)).unwrap();
            std::fs::write(manager.profile_dir(name).join(".claude.json"), auth).unwrap();
        }
        std::fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        let store = usage::store(&manager, None).unwrap();
        assert_eq!(
            reset_action(&manager, &store, "p", ResetCommand::Undo, now, offset).unwrap(),
            "No resets recorded for p.\n"
        );
        assert!(!store.dir.join("billing.json").exists());
        usage::billing::edit(&store, now, |billing| {
            billing.profiles.insert(
                "q".into(),
                usage::billing::ProfileBilling {
                    plan: Some(usage::billing::Plan {
                        label: "Q".into(),
                        fee_usd: 1.0,
                    }),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
        assert!(
            Cli::try_parse_from(["cswitch", "usage", "reset", "p", "--at", "2030-01-08"]).is_ok()
        );
        assert!(
            reset_action(
                &manager,
                &store,
                "k",
                ResetCommand::Record(Some("2030-01-08")),
                now,
                offset
            )
            .is_err()
        );
        assert!(
            reset_action(
                &manager,
                &store,
                "p",
                ResetCommand::Record(Some("2030-01-12")),
                now,
                offset
            )
            .is_err()
        );
        reset_action(
            &manager,
            &store,
            "p",
            ResetCommand::Record(Some("2030-01-08")),
            now,
            offset,
        )
        .unwrap();
        let settings = usage::billing::read(&store.dir, now).unwrap();
        let day = &settings.profiles["p"].limit_resets[0];
        assert_eq!(
            day.from,
            DateTime::parse_from_rfc3339("2030-01-08T03:00:00Z")
                .unwrap()
                .to_utc()
        );
        assert_eq!(
            day.to,
            DateTime::parse_from_rfc3339("2030-01-09T03:00:00Z")
                .unwrap()
                .to_utc()
        );
        assert!(settings.profiles.contains_key("q"));
        reset_action(
            &manager,
            &store,
            "p",
            ResetCommand::Record(Some("2030-01-08 14:30")),
            now,
            offset,
        )
        .unwrap();
        let settings = usage::billing::read(&store.dir, now).unwrap();
        let minute = &settings.profiles["p"].limit_resets[1];
        assert_eq!(minute.from, minute.to);
        assert_eq!(
            minute.from,
            DateTime::parse_from_rfc3339("2030-01-08T17:30:00Z")
                .unwrap()
                .to_utc()
        );
        let listed = reset_action(&manager, &store, "p", ResetCommand::List, now, offset).unwrap();
        assert!(
            listed.contains("1. 2030-01-08 00:00 to 2030-01-09 00:00"),
            "{listed}"
        );
        assert!(
            listed.contains("2. 2030-01-08 14:30 to 2030-01-08 14:30"),
            "{listed}"
        );
        reset_action(&manager, &store, "p", ResetCommand::Undo, now, offset).unwrap();
        assert_eq!(
            usage::billing::read(&store.dir, now).unwrap().profiles["p"]
                .limit_resets
                .len(),
            1
        );
        // Known-bads: append order persisted unsorted, undo removed oldest, default kept seconds.
        reset_action(
            &manager,
            &store,
            "q",
            ResetCommand::Record(Some("2030-01-10 10:00")),
            now,
            offset,
        )
        .unwrap();
        reset_action(
            &manager,
            &store,
            "q",
            ResetCommand::Record(Some("2030-01-07 09:00")),
            now,
            offset,
        )
        .unwrap();
        let q = &usage::billing::read(&store.dir, now).unwrap().profiles["q"].limit_resets;
        assert_eq!(q.len(), 2);
        assert!(q[0].from < q[1].from);
        assert_eq!(
            reset_action(&manager, &store, "q", ResetCommand::List, now, offset).unwrap(),
            "1. 2030-01-07 09:00 to 2030-01-07 09:00\n2. 2030-01-10 10:00 to 2030-01-10 10:00\n"
        );
        reset_action(&manager, &store, "q", ResetCommand::Undo, now, offset).unwrap();
        let q = &usage::billing::read(&store.dir, now).unwrap().profiles["q"].limit_resets;
        assert_eq!(q.len(), 1);
        assert_eq!(
            q[0].from,
            DateTime::parse_from_rfc3339("2030-01-07T12:00:00Z")
                .unwrap()
                .to_utc()
        );
        let now_with_seconds = now + chrono::Duration::seconds(37);
        reset_action(
            &manager,
            &store,
            "q",
            ResetCommand::Record(None),
            now_with_seconds,
            offset,
        )
        .unwrap();
        let q = &usage::billing::read(&store.dir, now_with_seconds)
            .unwrap()
            .profiles["q"]
            .limit_resets;
        let default = q.last().unwrap();
        assert_eq!(default.from, default.to);
        assert_eq!(default.from.timestamp() % 60, 0);
        let lock = store.try_lock().unwrap().unwrap();
        assert!(
            reset_action(
                &manager,
                &store,
                "p",
                ResetCommand::Record(None),
                now,
                offset
            )
            .unwrap_err()
            .to_string()
            .contains("busy")
        );
        drop(lock);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(store.dir.join("billing.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn purge_refusal_keeps_registry_and_success_removes_only_named_history() {
        // Known-bad: registry surviving only after ledger rows were already purged,
        // or a valid purge retaining old capacity rows for the removed name.
        let tmp = tempfile::tempdir().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = Utc::now();
        let mut registry = profile::Registry::default();
        for name in ["p", "q"] {
            registry.profiles.insert(
                name.into(),
                profile::Profile {
                    name: name.into(),
                    tool: Tool::Claude,
                    email: Some(format!("{name}@example.com")),
                    added: now,
                    last_used: None,
                },
            );
            std::fs::create_dir_all(manager.profile_dir(name)).unwrap();
        }
        let registry_path = manager.base_dir.join("registry.json");
        let registry_bytes = serde_json::to_vec(&registry).unwrap();
        std::fs::write(&registry_path, &registry_bytes).unwrap();
        let usage = manager.base_dir.join("usage");
        std::fs::create_dir_all(&usage).unwrap();
        let history = ["p", "q"]
            .iter()
            .map(|name| {
                serde_json::json!({
                    "profile":name,"fetched_at":"2030-01-03T00:00:00Z",
                    "weekly":{"percent":50,"resets_at":"2030-01-08T00:00:00Z"}
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(usage.join("limits.jsonl"), &history).unwrap();
        std::fs::write(usage.join("billing.json"), b"{bad").unwrap();
        assert!(
            remove_profile_with_usage(&manager, "p", true, false, None)
                .unwrap_err()
                .to_string()
                .contains("billing.json")
        );
        assert_eq!(std::fs::read(&registry_path).unwrap(), registry_bytes);
        assert_eq!(
            std::fs::read_to_string(usage.join("limits.jsonl")).unwrap(),
            history
        );
        assert!(manager.profile_dir("p").exists());
        std::fs::write(
            usage.join("billing.json"),
            r#"{"version":1,"profiles":{"p":{},"q":{}}}"#,
        )
        .unwrap();
        remove_profile_with_usage(&manager, "p", true, false, None).unwrap();
        assert!(manager.get_profile("p").is_err());
        assert!(manager.get_profile("q").is_ok());
        assert_eq!(
            usage::metrics::history(&usage)
                .iter()
                .map(|r| r.profile.as_str())
                .collect::<Vec<_>>(),
            vec!["q"]
        );
    }
    #[test]
    fn removed_topup_command_is_rejected() {
        // Known-bad: the removed topup command still parsing.
        assert!(Cli::try_parse_from(["cswitch", "usage", "topup", "p", "1"]).is_err());
    }
    #[test]
    fn flat_and_per_type_flags_conflict_at_parse_time() {
        // Known-bad: accepting both flat and per-type prices and silently choosing one.
        assert!(
            Cli::try_parse_from([
                "cswitch", "usage", "rate", "p", "--flat", "1", "--input", "2"
            ])
            .is_err()
        );
    }
    #[test]
    fn plan_usage_block_shows_fee_and_list_value_without_spend() {
        // Known-bad: treating a subscription's list value as spent or adding its fee per request.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let now = DateTime::parse_from_rfc3339("2030-01-08T12:00:00Z")
            .unwrap()
            .to_utc();
        let hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: now,
            rows: vec![usage::metrics::Bucket {
                profile: "p".into(),
                hour: now - chrono::Duration::hours(1),
                model: "claude-opus-5".into(),
                speed: None,
                requests: 1,
                input: 1_000_000,
                output: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
                cache_read: 0,
            }],
        };
        std::fs::write(
            dir.join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        let settings = usage::billing::Billing {
            version: 1,
            profiles: std::collections::BTreeMap::from([(
                "p".into(),
                usage::billing::ProfileBilling {
                    plan: Some(usage::billing::Plan {
                        label: "Pro".into(),
                        fee_usd: 2.0,
                    }),
                    ..Default::default()
                },
            )]),
        };
        std::fs::write(
            dir.join("billing.json"),
            serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        let text = usage_info(
            dir,
            "p",
            &key::AuthMode::Subscription,
            now,
            FixedOffset::east_opt(0).unwrap(),
            &Limits::NoSnapshot,
        );
        assert!(text.contains("$5.00 list price"), "{text}");
        assert!(
            text.contains("Pro · $2.00/mo · last 30 days $5.00 of list-price usage (2× the fee)"),
            "{text}"
        );
        assert!(
            text.contains("Effective: $2.00 per 1M tokens over 30 days (fee ÷ tokens)"),
            "{text}"
        );
        assert!(!text.contains("spent"), "{text}");
        let empty = usage::metrics::Hourly {
            rows: vec![],
            ..hourly
        };
        std::fs::write(dir.join("hourly.json"), serde_json::to_vec(&empty).unwrap()).unwrap();
        let no_tokens = usage_info(
            dir,
            "p",
            &key::AuthMode::Subscription,
            now,
            FixedOffset::east_opt(0).unwrap(),
            &Limits::NoSnapshot,
        );
        assert!(!no_tokens.contains("Effective:"), "{no_tokens}"); // Known-bad: fee ÷ zero tokens.
    }
    #[test]
    fn plan_capacity_line_shows_reset_range_and_waiting_reason() {
        // Known-bads: doubling the capacity in info, omitting the reset range,
        // or inverting the waiting-for-ingest condition.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let at = |text: &str| DateTime::parse_from_rfc3339(text).unwrap().to_utc();
        let now = at("2030-01-07T12:00:00Z");
        let bucket = |hour: &str| usage::metrics::Bucket {
            profile: "p".into(),
            hour: at(hour),
            model: "claude-opus-5".into(),
            speed: None,
            requests: 1,
            input: 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        };
        let mut hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: at("2030-01-07T00:00:00Z"),
            rows: vec![
                bucket("2030-01-02T00:00:00Z"),
                bucket("2030-01-05T00:00:00Z"),
                bucket("2030-01-06T00:00:00Z"),
            ],
        };
        std::fs::write(
            dir.join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        let reset = usage::billing::parse_reset_bracket(
            Some("2030-01-04"),
            now,
            FixedOffset::west_opt(3 * 3600).unwrap(),
        )
        .unwrap();
        let mut billing = usage::billing::Billing {
            version: 1,
            profiles: std::collections::BTreeMap::from([(
                "p".into(),
                usage::billing::ProfileBilling {
                    plan: Some(usage::billing::Plan {
                        label: "Pro".into(),
                        fee_usd: 20.0,
                    }),
                    limit_resets: vec![reset],
                    ..Default::default()
                },
            )]),
        };
        std::fs::write(
            dir.join("billing.json"),
            serde_json::to_vec(&billing).unwrap(),
        )
        .unwrap();
        let rows = [
            usage::metrics::LimitRow {
                profile: "p".into(),
                fetched_at: at("2030-01-03T00:00:00Z"),
                weekly: usage::metrics::Weekly {
                    percent: 100.0,
                    resets_at: at("2030-01-08T00:00:00Z"),
                    window_started_at: None,
                },
            },
            usage::metrics::LimitRow {
                profile: "p".into(),
                fetched_at: at("2030-01-06T00:00:00Z"),
                weekly: usage::metrics::Weekly {
                    percent: 60.0,
                    resets_at: at("2030-01-08T00:00:00Z"),
                    window_started_at: None,
                },
            },
        ];
        let history = rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(dir.join("limits.jsonl"), history).unwrap();
        let offset = FixedOffset::west_opt(3 * 3600).unwrap();
        let text = usage_info(
            dir,
            "p",
            &key::AuthMode::Subscription,
            now,
            offset,
            &Limits::NoSnapshot,
        );
        assert!(text.contains("Capacity:  weekly limit ≈ $8–$17 of list-price usage (est. from 60% at 01-05 21:00, after the reset on 01-04; last 4 estimates $5–$17)"),"{text}");
        std::fs::remove_file(dir.join("limits.jsonl")).unwrap();
        hourly.generated_at = at("2030-01-06T00:00:00Z");
        std::fs::write(
            dir.join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        let live = parse_limits(&serde_json::json!({"cachedUsageUtilization":{
            "fetchedAtMs":at("2030-01-07T00:00:00Z").timestamp_millis(),
            "utilization":{"limits":[{"kind":"weekly_all","group":"weekly","percent":50,
                "resets_at":"2030-01-08T00:00:00Z"}]}}}));
        let waiting = usage_info(dir, "p", &key::AuthMode::Subscription, now, offset, &live);
        assert!(
            waiting.contains("Capacity:  not enough data yet (waiting for an ingest)"),
            "{waiting}"
        );
        billing.profiles.get_mut("p").unwrap().limit_resets.clear();
        std::fs::write(
            dir.join("billing.json"),
            serde_json::to_vec(&billing).unwrap(),
        )
        .unwrap();
        let long = [
            usage::metrics::LimitRow {
                profile: "p".into(),
                fetched_at: at("2029-12-31T00:00:00Z"),
                weekly: usage::metrics::Weekly {
                    percent: 80.0,
                    resets_at: at("2030-01-08T00:00:00Z"),
                    window_started_at: None,
                },
            },
            usage::metrics::LimitRow {
                profile: "p".into(),
                fetched_at: at("2030-01-05T00:00:00Z"),
                weekly: usage::metrics::Weekly {
                    percent: 30.0,
                    resets_at: at("2030-01-08T00:00:00Z"),
                    window_started_at: None,
                },
            },
        ];
        let history = long
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(dir.join("limits.jsonl"), history).unwrap();
        let hinted = usage_info(
            dir,
            "p",
            &key::AuthMode::Subscription,
            now,
            offset,
            &Limits::NoSnapshot,
        );
        assert!(hinted.contains("Reset:     detected between 12-30 21:00 and 01-04 21:00 — record it with cswitch usage reset p --at <YYYY-MM-DD>"),"{hinted}");
    }
    #[test]
    fn equal_reset_capacity_ends_print_once_but_date_bracket_prints_range() {
        // Known-bad: minute-form reset renders $150–$150; date bracket loses its range.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let now = at("2030-01-07T12:00:00Z");
        let bucket = |s: &str| usage::metrics::Bucket {
            profile: "p".into(),
            hour: at(s),
            model: "claude-opus-5".into(),
            speed: None,
            requests: 1,
            input: 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        };
        fs::write(
            dir.join("hourly.json"),
            serde_json::to_vec(&usage::metrics::Hourly {
                version: 1,
                generated_at: now,
                rows: vec![
                    bucket("2030-01-04T13:00:00Z"),
                    bucket("2030-01-06T00:00:00Z"),
                ],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("limits.jsonl"),
            serde_json::json!({"profile":"p","fetched_at":"2030-01-06T00:00:00Z",
                "weekly":{"percent":50,"resets_at":"2030-01-08T00:00:00Z"}})
            .to_string()
                + "\n",
        )
        .unwrap();
        let render = |reset| {
            fs::write(
                dir.join("billing.json"),
                serde_json::to_vec(&usage::billing::Billing {
                    version: 1,
                    profiles: std::collections::BTreeMap::from([(
                        "p".into(),
                        usage::billing::ProfileBilling {
                            limit_resets: vec![reset],
                            ..Default::default()
                        },
                    )]),
                })
                .unwrap(),
            )
            .unwrap();
            usage_info(
                dir,
                "p",
                &key::AuthMode::Subscription,
                now,
                FixedOffset::east_opt(0).unwrap(),
                &Limits::NoSnapshot,
            )
        };
        let minute = at("2030-01-04T12:00:00Z");
        let point = render(usage::billing::LimitReset {
            from: minute,
            to: minute,
        });
        assert!(
            point.contains("weekly limit ≈ $20 of list-price usage"),
            "{point}"
        );
        assert!(point.contains("after the reset on 01-04"), "{point}");
        assert!(!point.contains("weekly limit ≈ $20–$20"), "{point}");
        let range = render(usage::billing::LimitReset {
            from: at("2030-01-04T00:00:00Z"),
            to: at("2030-01-05T00:00:00Z"),
        });
        assert!(
            range.contains("weekly limit ≈ $10–$20 of list-price usage"),
            "{range}"
        );
    }
    #[test]
    fn effective_rate_needs_priced_spend_and_marks_partial_spend() {
        // Known-bads: $0.00* when all tokens are unpriced; dropping * for mixed spend.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let now = DateTime::parse_from_rfc3339("2030-01-08T12:00:00Z")
            .unwrap()
            .to_utc();
        let bucket = |model: &str| usage::metrics::Bucket {
            profile: "p".into(),
            hour: now - chrono::Duration::hours(1),
            model: model.into(),
            speed: None,
            requests: 1,
            input: 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        };
        fs::write(
            dir.join("billing.json"),
            serde_json::json!({"version":1,"profiles":{"p":{"rate":{"model_prefixes":["acme/priced"],"flat":1.0}}}}).to_string(),
        )
        .unwrap();
        let render = |rows| {
            fs::write(
                dir.join("hourly.json"),
                serde_json::to_vec(&usage::metrics::Hourly {
                    version: 1,
                    generated_at: now,
                    rows,
                })
                .unwrap(),
            )
            .unwrap();
            usage_info(
                dir,
                "p",
                &key::AuthMode::ApiKey(None),
                now,
                FixedOffset::east_opt(0).unwrap(),
                &Limits::NoSnapshot,
            )
        };
        let unpriced = render(vec![bucket("acme/unknown")]);
        assert!(!unpriced.contains("Effective:"), "{unpriced}");
        let mixed = render(vec![bucket("acme/unknown"), bucket("acme/priced")]);
        assert!(
            mixed.contains("Effective: $0.50* per 1M tokens over 30 days"),
            "{mixed}"
        );
    }
    #[test]
    fn effective_spend_and_rate_line_keep_subdollar_precision() {
        // Known-bad: rounding rate 0.135 to cents or dividing by a million twice.
        let tmp = tempfile::tempdir().unwrap();
        let now = DateTime::parse_from_rfc3339("2030-01-08T12:00:00Z")
            .unwrap()
            .to_utc();
        let mut hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: now,
            rows: vec![usage::metrics::Bucket {
                profile: "p".into(),
                hour: now - chrono::Duration::hours(1),
                model: "acme/model".into(),
                speed: None,
                requests: 1,
                input: 1_000_000,
                output: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
                cache_read: 0,
            }],
        };
        std::fs::write(
            tmp.path().join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        let billing = usage::billing::Billing {
            version: 1,
            profiles: std::collections::BTreeMap::from([(
                "p".into(),
                usage::billing::ProfileBilling {
                    rate: Some(usage::billing::Rate {
                        model_prefixes: vec!["acme/".into()],
                        price: usage::billing::RatePrice::Flat { flat: 0.135 },
                    }),
                    ..Default::default()
                },
            )]),
        };
        std::fs::write(
            tmp.path().join("billing.json"),
            serde_json::to_vec(&billing).unwrap(),
        )
        .unwrap();
        let render = || {
            usage_info(
                tmp.path(),
                "p",
                &key::AuthMode::ApiKey(None),
                now,
                FixedOffset::east_opt(0).unwrap(),
                &Limits::NoSnapshot,
            )
        };
        let text = render();
        assert!(
            text.contains("Rate:      $0.135 per 1M tokens, flat"),
            "{text}"
        );
        assert!(
            text.contains("Effective: $0.135 per 1M tokens over 30 days (spend ÷ tokens)"),
            "{text}"
        );
        std::fs::write(
            tmp.path().join("billing.json"),
            serde_json::json!({"version":1,"profiles":{"p":{"rate":{
                "input":0.1,"output":0.2,"cache_write_5m":0.3,
                "cache_write_1h":0.4,"cache_read":0.075}}}})
            .to_string(),
        )
        .unwrap();
        let typed = render();
        assert!(typed.contains("read $0.075"), "{typed}"); // Known-bad: money() rounds the rate to cents.
        hourly.rows.clear();
        std::fs::write(
            tmp.path().join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        assert!(!render().contains("Effective:")); // Known-bad: dividing by zero tokens.
    }
}

#[cfg(test)]
mod billing_list_variants_tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;
    #[test]
    fn ten_day_bucket_counts_in_thirty_day_list_and_info_only() {
        // Known-bad: the 30D list cell reading the seven-day window.
        let tmp = tempfile::tempdir().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = Utc.with_ymd_and_hms(2030, 2, 1, 12, 0, 0).unwrap();
        let mut registry = profile::Registry::default();
        registry.profiles.insert(
            "p".into(),
            profile::Profile {
                name: "p".into(),
                tool: Tool::Claude,
                email: Some("p@example.com".into()),
                added: now,
                last_used: None,
            },
        );
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(manager.profile_dir("p")).unwrap();
        fs::write(
            manager.profile_dir("p").join(".claude.json"),
            r#"{"oauthAccount":{}}"#,
        )
        .unwrap();
        let usage = manager.base_dir.join("usage");
        fs::create_dir_all(&usage).unwrap();
        let hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: now,
            rows: vec![usage::metrics::Bucket {
                profile: "p".into(),
                hour: now - chrono::Duration::days(10),
                model: "claude-opus-5".into(),
                speed: None,
                requests: 1,
                input: 1_000_000,
                output: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
                cache_read: 0,
            }],
        };
        fs::write(
            usage.join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        let list = list_output(&manager, now).unwrap();
        let info = info_output(&manager, "p", now).unwrap();
        assert!(
            list.lines()
                .any(|line| line.starts_with("p ") && line.contains("~$5.00")),
            "{list}"
        );
        let seven = info
            .lines()
            .find(|line| line.trim_start().starts_with("7 days"))
            .unwrap();
        let thirty = info
            .lines()
            .find(|line| line.trim_start().starts_with("30 days"))
            .unwrap();
        assert!(seven.contains("— list price"), "{seven}");
        assert!(thirty.contains("$5.00 list price"), "{thirty}");
    }
    #[test]
    fn wide_list_and_every_money_cell_variant_fit_120_columns() {
        // Known-bad: a 121-column row or assuming every Claude request has a price.
        let tmp = tempfile::tempdir().unwrap();
        let manager =
            ProfileManager::with_paths(tmp.path().join("switch"), tmp.path().join(".claude"))
                .unwrap();
        let now = Utc.with_ymd_and_hms(2030, 1, 8, 12, 0, 0).unwrap();
        let long = "abcdefghijklmnopqrst";
        let mut registry = profile::Registry::default();
        for (name, auth) in [
            (long, "{\"primaryApiKey\":\"synthetic\"}"),
            ("keyed", "{\"primaryApiKey\":\"synthetic\"}"),
            ("plan", "{\"oauthAccount\":{}}"),
            ("other", "{}"),
        ] {
            registry.profiles.insert(
                name.into(),
                profile::Profile {
                    name: name.into(),
                    tool: Tool::Claude,
                    email: Some("forty.characters.long.address@example.com".into()),
                    added: now,
                    last_used: None,
                },
            );
            fs::create_dir_all(manager.profile_dir(name)).unwrap();
            fs::write(manager.profile_dir(name).join(".claude.json"), auth).unwrap();
        }
        // Known-bad: a fresh snapshot says "seconds ago", overflowing a 10-wide AS OF column.
        fs::write(
            manager.profile_dir("plan").join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {},
                "cachedUsageUtilization": {
                    "fetchedAtMs": now.timestamp_millis() - 30_000,
                    "utilization": {"limits": [{"kind":"weekly_all", "group":"weekly", "percent":50,
                        "resets_at":"2030-01-10T12:00:00Z"}]}
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            manager.base_dir.join("registry.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();
        let usage = manager.base_dir.join("usage");
        fs::create_dir_all(&usage).unwrap();
        let row = |profile: &str, model: &str| usage::metrics::Bucket {
            profile: profile.into(),
            hour: now - chrono::Duration::hours(1),
            model: model.into(),
            speed: None,
            requests: 1,
            input: 1_000_000,
            output: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        };
        let mut plan_row = row("plan", "claude-opus-5");
        plan_row.input = 2_469_120_000; // Known-bad: $12,345.60 rendered with cents or no comma.
        let hourly = usage::metrics::Hourly {
            version: 1,
            generated_at: now,
            rows: vec![
                row(long, "unknown"),
                row("keyed", "acme/claude-x.5"),
                plan_row,
                row("other", "claude-opus-5"),
                row("other", "unknown"),
            ],
        };
        fs::write(
            usage.join("hourly.json"),
            serde_json::to_vec(&hourly).unwrap(),
        )
        .unwrap();
        let mut rates = usage::rates::seed();
        rates
            .aliases
            .insert("acme/claude-x.5".into(), "claude-opus-5".into());
        fs::write(
            usage.join("rates.json"),
            serde_json::to_vec(&rates).unwrap(),
        )
        .unwrap();
        fs::write(usage.join("billing.json"),r#"{"version":1,"profiles":{"keyed":{"rate":{"model_prefixes":["acme/"],"flat":1.0}}}}"#).unwrap();
        let out = list_output(&manager, now).unwrap();
        assert!(out.lines().all(|line| line.chars().count() <= 120), "{out}");
        assert!(
            out.lines()
                .any(|line| line.starts_with(long) && line.contains("$*")),
            "{out}"
        );
        assert!(
            out.lines()
                .any(|line| line.starts_with("plan ") && line.contains("~$12,346")),
            "{out}"
        );
        let keyed = out.lines().find(|line| line.starts_with("keyed ")).unwrap();
        assert!(
            keyed.contains("$1.00") && !keyed.contains("$5.00"),
            "{keyed}"
        ); // Known-bad: per-token cell using list value.
        assert!(
            out.lines()
                .any(|line| line.starts_with("other ") && line.contains("~$5.00*")),
            "{out}"
        );
        assert!(out.contains("30D $ = last 30 days"));
    }
}
