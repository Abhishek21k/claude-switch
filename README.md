# claude-switch

Multi-account profile manager for [Claude Code](https://docs.anthropic.com/en/docs/claude-code).

Switch between multiple Claude accounts without logging out. Each profile is fully isolated — run different accounts in different terminals simultaneously.

## Why

Claude Code ties one account to `~/.claude`. If you have a work account, a personal account, or a client account, you have to log out and back in every time you switch. claude-switch eliminates that entirely.

## Install

### Homebrew (macOS/Linux)

```bash
brew install Abhishek21k/tap/cc-switch
```

### Cargo (requires Rust)

```bash
cargo install cswitch
```

### Pre-built binaries

Download the latest binary for your platform from [GitHub Releases](https://github.com/Abhishek21k/claude-switch/releases).

```bash
# macOS (Apple Silicon)
curl -fsSL https://github.com/Abhishek21k/claude-switch/releases/latest/download/cc-switch-aarch64-apple-darwin.tar.gz | tar xz
sudo mv cswitch /usr/local/bin/

# macOS (Intel)
curl -fsSL https://github.com/Abhishek21k/claude-switch/releases/latest/download/cc-switch-x86_64-apple-darwin.tar.gz | tar xz
sudo mv cswitch /usr/local/bin/

# Linux
curl -fsSL https://github.com/Abhishek21k/claude-switch/releases/latest/download/cc-switch-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv cswitch /usr/local/bin/
```

### From source

```bash
git clone https://github.com/Abhishek21k/claude-switch.git
cd claude-switch
cargo install --path .
```

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
| `cswitch login <name>` | Create a profile by logging into a different account |
| `cswitch login <name> --email <addr>` | Same, pre-filling the address on Claude's login page |
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
┌ ↑↓/jk nav  enter launch  / search  a add account ...  ┐
```

### TUI keybindings

| Key | Action |
|---|---|
| `↑/↓` or `j/k` | Navigate profiles |
| `Enter` | Launch Claude with selected profile |
| `/` | Search profiles by name or email |
| `a` | Add account — enter a name, then choose copy or login |
| `l` | Login — shortcut straight to a different account |
| `r` | Refresh — overwrite the selected profile with the current session (confirmed) |
| `d` | Delete selected profile |
| `?` | Help overlay |
| `q` / `Esc` | Quit |

## Copy vs Login

A profile is a **config environment**, not an identity. Its name is a local label you choose; the Claude account inside it comes from authentication and nothing else. So `a` always asks which of two things you want:

**Copy current session** — same Claude account, separate setup.

```bash
cswitch add review      # → choose [c]
```

Use this for one account with two environments: different MCP servers, different project trust, separate conversation history. `review` and your main profile stay the same account.

**Login to a different Claude account** — a different identity.

```bash
cswitch login business   # or press `a`, then [l]
```

The new profile is seeded with your warm setup (settings, skills, project trust), then every trace of the old account is stripped so Claude has to authenticate from scratch.

### The browser decides which account you get

`claude auth login` hands account selection to your browser. If claude.ai is already signed in as another account, OAuth grants **that** account — usually with no picker — and you end up with a second profile for the account you already had.

Before logging a different account in:

- sign out of claude.ai, **or**
- complete the login in a private/incognito window.

`cswitch login <name> --email you@company.com` pre-fills the login page, which helps once you are signed out. It cannot override a live session.

After every login, cswitch reports the account Claude actually authenticated as — read from `claude auth status`, never from the name you typed. If that account already belongs to another profile, it says so instead of pretending a new account was added.

### Two profiles, one account

This is allowed, not an error. Duplicate account identity is a warning because separate config environments for a single Claude account are a legitimate setup. If it was not what you meant, the fix is in the browser:

```bash
cswitch remove business
# sign out of claude.ai, then:
cswitch login business
```

## Adding your first profile

When you run `cswitch` for the first time, it detects your active Claude session and offers two options:

1. **Copy active session** — saves your current credentials as a profile, no re-login needed
2. **Login to a new account** — opens Claude so you can authenticate with a different account

After that, `a` in the TUI offers the same two choices for every additional profile.

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
