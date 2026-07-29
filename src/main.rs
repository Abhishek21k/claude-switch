mod profile;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use profile::{LoginOutcome, ProfileManager, detect_current_account};
use std::io::{self, Write};

#[derive(Parser)]
#[command(
    name = "cswitch",
    about = "Multi-account profile manager for Claude Code",
    long_about = "Manage multiple Claude Code accounts using isolated config directories.\n\
                  Each profile stores a complete ~/.claude snapshot and launches Claude\n\
                  with CLAUDE_CONFIG_DIR set — no credential swapping, no side effects.",
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
    Use {
        /// Profile name to use
        name: String,
        /// Extra arguments passed directly to claude
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
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
    let cli = Cli::parse();
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
                println!("  CLAUDE_CONFIG_DIR='{}' claude", dir.display());
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
