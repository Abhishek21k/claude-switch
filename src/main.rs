mod profile;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use profile::{detect_current_account, ProfileManager};
use std::io::{self, Write};

#[derive(Parser)]
#[command(
    name = "cswitch",
    about = "Multi-account profile manager for Claude Code",
    long_about = "Manage multiple Claude Code accounts using isolated config directories.\n\
                  Each profile stores a complete ~/.claude snapshot and launches Claude\n\
                  with CLAUDE_CONFIG_DIR set — no credential swapping, no side effects.",
    version
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
    },

    /// Log in to a new Claude account and save it as a profile (skips detection prompt)
    Login {
        /// Profile name (alphanumeric, hyphens, underscores)
        name: String,
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

            println!("{:<20} {:<35} {}", "NAME", "EMAIL", "LAST USED");
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

        Some(Commands::Add { name, force }) => {
            handle_add(&manager, &name, force)?;
        }

        Some(Commands::Login { name }) => {
            manager.login_profile(&name)?;
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
fn handle_add(manager: &ProfileManager, name: &str, force: bool) -> Result<()> {
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
                        manager.add_profile_force(name)
                    } else {
                        manager.add_profile(name)
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
                    manager.login_profile(name)?;
                }
                _ => unreachable!(),
            }
        }
        None => {
            // No active session — go straight to login
            println!("No active Claude session found. Opening Claude for login…\n");
            manager.login_profile(name)?;
        }
    }
    Ok(())
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
        println!("Please enter one of: {}", valid.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", "));
    }
}
