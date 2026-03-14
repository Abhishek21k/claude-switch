# claude-switch

Multi-account profile manager for [Claude Code](https://docs.anthropic.com/en/docs/claude-code).

Switch between multiple Claude accounts without logging out. Each profile is fully isolated — run different accounts in different terminals simultaneously.

## Why

Claude Code ties one account to `~/.claude`. If you have a work account, a personal account, or a client account, you have to log out and back in every time you switch. claude-switch eliminates that entirely.

## Install

### From source (requires Rust)

```bash
git clone https://github.com/YOUR_USERNAME/claude-switch.git
cd claude-switch
cargo install --path .
```

### Pre-built binaries

Coming soon. Check [Releases](https://github.com/YOUR_USERNAME/claude-switch/releases).

## Quick start

```bash
# Save your currently logged-in account as a profile
cswitch add work

# Add another account (opens Claude for you to log in)
cswitch login personal

# Switch between them
cswitch use work
cswitch use personal

# Or just open the interactive TUI
cswitch
```

## Commands

| Command | Description |
|---|---|
| `cswitch` | Open interactive TUI |
| `cswitch add <name>` | Add a new profile (detects active session, asks to copy or login) |
| `cswitch login <name>` | Create a profile by logging into a new account |
| `cswitch use <name>` | Launch Claude Code with a specific profile |
| `cswitch list` | List all saved profiles |
| `cswitch info <name>` | Show details for a profile |
| `cswitch remove <name>` | Delete a profile |
| `cswitch aliases` | Print shell aliases for all profiles |
| `cswitch --help` | Full CLI help |

## Interactive TUI

Run `cswitch` with no arguments to open the TUI.

```
┌─ ◆ claude-switch  profile manager ──────── 3 profiles ┐
┌─ Profiles ────────┐┌─ Details ─────────────────────────┐
│ ▶ work            ││  Name       work                  │
│   work@co.com     ││  Email      work@co.com           │
│                   ││  Added      2025-03-15 10:30 UTC  │
│   personal        ││  Last used  2025-03-15 14:22 UTC  │
│   me@gmail.com    ││                                   │
│                   ││  Launch command                    │
│   client          ││  CLAUDE_CONFIG_DIR='...' claude   │
│   dev@client.io   ││                                   │
└───────────────────┘└───────────────────────────────────┘
┌ ↑↓/jk nav  enter launch  / search  l login  a add ... ┐
```

### TUI keybindings

| Key | Action |
|---|---|
| `↑/↓` or `j/k` | Navigate profiles |
| `Enter` | Launch Claude with selected profile |
| `/` | Search profiles by name or email |
| `l` | Login — add a new account |
| `a` | Add — copy current session as a profile |
| `r` | Refresh — re-copy current session into selected profile |
| `d` | Delete selected profile |
| `?` | Help overlay |
| `q` / `Esc` | Quit |

## Adding your first profile

When you run `cswitch` for the first time, it detects your active Claude session and offers two options:

1. **Copy active session** — saves your current credentials as a profile, no re-login needed
2. **Login to a new account** — opens Claude so you can authenticate with a different account

For every additional account after the first, use `cswitch login <name>`.

## Shell aliases

Generate aliases so you can launch profiles directly without `cswitch use`:

```bash
cswitch aliases >> ~/.zshrc   # or ~/.bashrc
source ~/.zshrc
```

This gives you commands like:

```bash
claude-work       # launches Claude with the "work" profile
claude-personal   # launches Claude with the "personal" profile
```

On Windows, `cswitch aliases` outputs PowerShell functions instead. Add them to your `$PROFILE`.

## Platform support

| | macOS | Linux | Windows |
|---|---|---|---|
| Profile management | Yes | Yes | Yes |
| Credential handling | Keychain | File-based | Credential Manager |
| Shell aliases | bash/zsh | bash/zsh | PowerShell |
| TUI | Yes | Yes | Yes |

## How profiles are stored

Profiles live in `~/.claude-switch/profiles/<name>/`. Each profile is a self-contained Claude Code config directory. When you run `cswitch use <name>`, it simply sets `CLAUDE_CONFIG_DIR` to point at that directory — Claude reads its credentials and config from there instead of the default `~/.claude`.

Nothing in your original `~/.claude` is modified. Profiles are fully isolated from each other.

## Running multiple accounts simultaneously

Open separate terminals and run different profiles in each:

```bash
# Terminal 1
cswitch use work

# Terminal 2
cswitch use personal
```

Both run independently with their own credentials and config.

## License

MIT
