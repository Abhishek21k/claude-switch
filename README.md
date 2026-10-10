# claude-switch

Multi-account profile manager for [Claude Code](https://docs.anthropic.com/en/docs/claude-code) and Codex.

Switch between accounts without logging out. Each profile has its own tool home, so accounts can run in different terminals simultaneously.

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

# Add a Codex (ChatGPT) account in its own home
cswitch login coding --tool codex

# Add an Antigravity (Google) account in its own HOME (Unix)
cswitch login research --tool antigravity

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
| `cswitch login <name> --console` | Create a profile through Anthropic Console (API billing) |
| `cswitch login <name> --tool codex` | Log in to Codex with a new ChatGPT session |
| `cswitch add <name> --tool codex` | Same Codex login, without a Copy option |
| `cswitch login <name> --tool antigravity` | Log in through Antigravity's own sign-in (Unix) |
| `cswitch add <name> --tool agy` | Same Antigravity login; `agy` is an alias for `antigravity` |
| `cswitch key set <name> [--replace-helper]` | Read a hidden key, then optionally configure an Anthropic-compatible gateway |
| `cswitch key clear <name>` | Remove the saved key and its managed gateway settings |
| `cswitch gateway list` | Show saved gateway URLs and setting names, without values |
| `cswitch gateway forget <url>` | Forget defaults for a gateway without editing any profile |
| `cswitch use <name> [tool flags...]` | Launch the profile's tool; Claude profiles sync skills first, and flags pass through unchanged |
| `cswitch sync <name> [--dry-run] [--adopt <skill>]...` | Sync shared skills into one profile |
| `cswitch sync --all [--dry-run] [--adopt <skill>]...` | Sync shared skills into every Claude profile |
| `cswitch list` | List all saved profiles |
| `cswitch info <name>` | Show details for a profile |
| `cswitch remove <name> [--purge-usage] [--force]` | Delete a profile; Antigravity local HOME entries require `--force`; keep usage unless explicitly purged |
| `cswitch usage` | Ingest local transcripts and show a 7-day token and API-equivalent cost report |
| `cswitch usage refresh` | Refresh the local ledger without printing a report |
| `cswitch usage label <session> <project>` | Label a past session without reopening it |
| `cswitch usage verify` | Check matching transcript tokens against Claude Code's cost-state snapshot |
| `cswitch statusline` | Print one Claude Code status line from stdin and local profile data |
| `cswitch statusline --install <name> [--no-refresh] [--force]` | Install the line for a registered Claude profile; `--all` selects every Claude profile |
| `cswitch statusline --uninstall <name>` | Remove a cswitch-owned line; `--all` selects every Claude profile |
| `cswitch aliases` | Print shell aliases for all profiles |
| `cswitch --help` | Full CLI help |

## API key

Create a profile with `cswitch login <name> --console`, or choose `[p]` after `cswitch add <name>` or in the TUI Add menu. Console login uses API billing. Later, `cswitch key set <name>` reads your own Anthropic key without echoing it. The key is never a command argument. A pipe can provide one line on stdin; piped key rotation leaves the gateway unchanged.

On a terminal, `key set` then asks for a gateway base URL or the provider's settings JSON. Paste the URL to reuse saved defaults for it, paste JSON to apply its non-credential settings and choose whether to save them, press Enter to keep the current gateway, or type `none` for the Anthropic API. The gateway input is hidden because provider JSON can contain a real token. For example, this synthetic snippet sets a base URL and model:

```json
{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example.com/anthropic","ANTHROPIC_MODEL":"vendor/claude-model[1m]","ANTHROPIC_AUTH_TOKEN":"TESTKEY"}}
```

`ANTHROPIC_AUTH_TOKEN` and other credential fields in pasted JSON are ignored and reported by name. The key comes only from the first prompt. The key is stored outside the profile at `~/.claude-switch/keys/<name>.key` (`0700` directory, `0600` file on Unix); it never enters `settings.json`. The profile's `settings.json` holds the `apiKeyHelper` command, base URL and safe gateway settings. Saved defaults live in the private `~/.claude-switch/gateways.json`; `cswitch gateway list` shows only URLs and setting names, and `cswitch gateway forget <url>` removes an entry without changing profiles.

The helper overrides a Console login or subscription while present. `cswitch key clear <name>` removes the helper and the base URL together, so the profile's saved login never goes to the gateway. If an unrelated helper already exists, `key set` refuses to replace it unless you pass `--replace-helper`.

`key print` is a hidden command used only by Claude Code's helper. It refuses to print to a terminal and refuses an exposed key file on Unix. `cswitch info` and the TUI show the active authentication source; API-billed rows in `cswitch list` show `api` instead of plan limits.

## Codex profiles

Run `cswitch login <name> --tool codex` or `cswitch add <name> --tool codex`, or choose `[o]` in the TUI Add menu. Each Codex profile directory is its `CODEX_HOME`. `cswitch use <name>` launches Codex with that directory and forwards any arguments. Profile names share one namespace across all tools.

Before login, cswitch copies only `config.toml`, `AGENTS.md`, `agents/`, `rules/` and `skills/` from `CODEX_HOME` (if set and non-empty) or `~/.codex`. It preserves links, but never copies `auth.json`, sessions, logs, sqlite state, secrets or daemon packages. The new login is verified by exit status; the status command's output is discarded because API-key mode can print part of a key. Account email and optional plan type are read offline from the new profile's `auth.json`.

Codex profiles cannot be copied or refreshed from a Claude session. Claude API keys, gateways, skills sync, plan limits and `cswitch usage` apply only to Claude profiles. `cswitch list` shows a `TOOL` column, while `info` and TUI details show the tool and any Codex plan type.

## Antigravity profiles

On Unix, run `cswitch login <name> --tool antigravity` or `cswitch add <name> --tool agy`, or choose `[g]` in the TUI Add menu. Sign in through the launched `agy` session, then exit it. cswitch checks the new profile's token file and runs `agy models` before registering the profile. This login needs `agy` to keep its sign-in in a file under the profile's `.gemini`, as measured on Linux under WSL. Each profile has its own `HOME` at `~/.claude-switch/profiles/<name>/home/`, so separate Google accounts can run in parallel.

The profile HOME links each top-level entry of your real HOME except `.gemini` and `.claude-switch`. Its `.gemini` is a real, isolated directory. cswitch copies the settings file, both MCP configuration files, the three skills directories, and Antigravity's migration marker from your real `.gemini`; it never copies the source login token or Gemini CLI account files. The marker keeps Antigravity from moving the copied files on its first start in the profile. `cswitch use <name>` updates the links and launches `agy` with the profile HOME. A file that Antigravity creates at the top of that HOME stays in the profile; cswitch leaves local entries alone. A program that rewrites a top-level file by renaming a new one over it replaces the link too. The profile then has its own copy, and the real file no longer changes with it. `cswitch info <name>` lists that copy under `Local:`.

Local entries exist only in the profile, and removing the profile deletes them. `cswitch remove <name>` lists them and requires `--force`; the TUI lists them before you confirm deletion. Inside an Antigravity session, `~/.claude-switch` is not linked, so `cswitch` cannot see its profiles there. It says so and stops; run `cswitch` from a normal shell. Antigravity profiles are not supported on Windows in this stage.

## Plan limits

`cswitch list`, `cswitch info <name>`, and the TUI details panel show the 5-hour and 7-day plan limits from Claude Code's own cached snapshot. Each view shows when Claude Code fetched it, and marks a window as `reset` once its reset time has passed. A new profile may show `no data` until Claude Code writes a snapshot. `cswitch` reads the profile's cache without changing it, makes no network call, and never refreshes OAuth tokens.

## Status line

`cswitch statusline` prints one line for Claude Code's status row. These layout examples use invented values:

```text
work · acme › site · 5h 42% ↻1h20 · 7d 71% ↻Thu · ctx 142k · chat ~$1.70 · today ~$23.10
work · acme › site · 5h 88% ↻20m · 7d 64% ↻Fri · → spare 5h 10% · ctx 142k
work · site · 5h 42% · 7d 71%
```

The first fields identify the account and project; an internal `(unattributed)` workspace is omitted. `5h` and `7d` use Claude Code's live plan utilization and local reset time. If stdin has no limits, a subscription profile uses its saved snapshot (`~/.claude.json` for `default`), and figures older than five minutes show their age. A per-token profile has no saved-limit fallback. `→` shows the registered subscription account with the most 5-hour headroom when the current account reaches 80% on either limit. It never points to an account whose active 5-hour or weekly window rounds to 100% or more. Its saved figure also shows its age. `ctx` is the latest context size.

`chat ~$` is Claude Code's running estimate on a subscription profile, and `today ~$` is today's ledger list value plus this chat's unrecorded live cost. On a per-token profile, `chat $` and `today $` are priced from the ledger and the configured rate, without `~`. `today —` means there is no priced request for today yet, and a trailing `*` means some requests have no rate. A stale `today` value shows its age. A config directory that is neither a registered Claude profile nor the default shows no ledger figure; its `chat` still uses Claude Code's live estimate. On a narrow terminal, the line drops ages, `today`, `chat`, reset times and `ctx`, shortens the project, then drops headroom and limits as needed; it never wraps. Set `NO_COLOR` to a non-empty value to disable yellow and red limit highlighting.

```bash
cswitch statusline --install work
cswitch statusline --install --all
cswitch statusline --install work --force
cswitch statusline --install work --no-refresh
cswitch statusline --uninstall work
cswitch statusline --uninstall --all
```

Install writes only the profile's `settings.json` key `"statusLine": { "type": "command", "command": "'<absolute path to cswitch>' statusline" }`; `--no-refresh` appends that flag to the command. The path is the resolved executable path and shell-quoted. When an existing ledger is over five minutes old, its per-chat rollup is missing, or the timestamps in `hourly.json` and `chats.json` disagree, the line prints first and starts one detached `cswitch usage refresh`, throttled to one attempt per minute. A timestamp mismatch hides ledger figures until that refresh finishes. `cswitch statusline --install <name> --no-refresh` installs the same display without background refresh. Install backs up the existing settings file before replacing it, preserves unrelated settings, and refuses another command's status line unless you pass `--force`. Uninstall removes only a cswitch-owned status line, including one installed by the same executable under another file name. Copied Claude profiles keep this portable command.

## Token usage

`cswitch usage` reads complete JSONL lines from the default Claude Code directory and registered profiles, then keeps deduplicated request rows in `~/.claude-switch/usage/`. It stores token counters, model IDs, timestamps, session titles, and project signals; it does not store prompts or tool inputs. `hourly.json` holds hourly totals, and `chats.json` holds per-session totals and one project signal per session. Its dollar column is an **API-equivalent weight**, not a subscription bill. Unknown models and fast requests without an explicit fast rate show `$*`. The editable `rates.json` is seeded once and never overwritten. Ingest makes no network call and does not change Claude Code files. The [ccusage Claude adapter notes](https://github.com/ccusage/ccusage/blob/main/rust/adapters/claude/src/README.md) describe the transcript layout and sidechain replay behavior used here.

```bash
cswitch usage --since 30d --by project
cswitch usage --since all --profile work --json
cswitch usage --explain <session-id>
cswitch usage --unattributed
cswitch usage refresh
cswitch usage label <session-id> <workspace/project>
cswitch usage verify
cswitch usage alias 'acme/claude-x.5' claude-opus-5-5
cswitch usage alias 'acme/claude-x.5' --remove
cswitch usage plan work 20 --label Pro
cswitch usage reset work --at 2030-01-08
cswitch usage reset work --at '2030-01-08 14:30'
cswitch usage reset work --list
cswitch usage reset work --undo
cswitch usage rate keyed --flat 1 --model-prefix 'acme/'
cswitch usage rate keyed --input 1 --output 5 --cache-write-5m 1.25 --cache-write-1h 2 --cache-read 0.1
```

`--since` accepts `7d` (default), `30d`, `all`, or `YYYY-MM-DD`; `--by` accepts `profile`, `workspace`, `project`, `session`, `model`, or `day`. The default view is a workspace › project › session tree. Set `CSWITCH_USAGE_DIR` to use another ledger directory, for example when ingesting into a temporary directory. The ledger survives profile removal; `cswitch remove <name> --purge-usage` opts into deleting that profile's ledger rows, billing settings and limit history.

`cswitch list` includes `30D $`: per-token spend or `~` list-price value for other Claude profiles. `cswitch info` shows today, 7-day and 30-day token totals, estimated spend and list value, an owner-entered plan fee, and a weekly capacity estimate. After a configured plan fee or per-token rate, `Effective:` divides that fee or the 30-day spend by the 30-day token count and shows USD per million tokens. It is absent when there are no tokens. Both views read the last offline hourly rollup; run `cswitch usage` to refresh. An absent ledger shows `—` and a refresh hint. `$*` marks unpriced usage. These estimates use the owner's settings and transcript counters; they are not gateway balance queries.

For example, a synthetic plan can show:

```text
Effective: $0.0375 per 1M tokens over 30 days (fee ÷ tokens)
Capacity:  weekly limit ≈ $380–$450 of list-price usage (est. from 70% at 01-10 17:00, after the reset on 01-08; last 4 estimates $360–$470)
```

List prices stay in `rates.json`. Its optional `aliases` object maps exact transcript model names to existing rate model IDs, for example `"acme/claude-x.5": "claude-opus-5-5"`. No name normalization occurs; unaliased names remain unpriced. `usage verify` skips aliased requests because Claude Code's cost state cannot price them.

Owner billing settings live in private `~/.claude-switch/usage/billing.json`, created atomically with mode `0600`. Rates are USD per million tokens. A rate applies only to its listed model prefixes; without prefixes it applies to every model. Other models use list price. Example with synthetic values:

```json
{
  "version": 1,
  "profiles": {
    "work": {
      "plan": { "label": "Pro", "fee_usd": 20.0 },
      "limit_resets": [ { "from": "2030-01-08T03:00:00Z", "to": "2030-01-09T03:00:00Z" } ]
    },
    "keyed": { "rate": { "model_prefixes": ["acme/"], "flat": 1.0 } }
  }
}
```

`cswitch usage reset` records a free weekly plan-limit reset when Claude Code's weekly percentage returns to zero without moving the scheduled reset time. Use `--at YYYY-MM-DD` when only the local day is known: cswitch records the whole local day as a bracket. `--at 'YYYY-MM-DD HH:MM'` records an exact local minute; omitting `--at` uses the current minute. `--list` shows the stored brackets, and `--undo` removes the newest. Only subscription profiles accept this command. A date bracket produces a capacity range because the reset could have happened at any time that day.

Weekly capacity assumes the window starts at Claude Code's `window_started_at` when present, or seven days before `resets_at` otherwise. Each drop of at least one percentage point between snapshots is its own detected reset, even when two drops share a snapshot; a recorded bracket can narrow one detected reset. Each reset splits the weekly window into segments. For each segment, cswitch uses the highest snapshot already covered by the ledger, starting at 20% utilization, and divides that segment's list-price usage by the utilization fraction. A long detected bracket needs a recorded reset before its post-reset segment can be estimated. `info` prints the newest eligible estimate and the min–max of the newest four estimates, in whole dollars; unpriced models mark an estimate partial. The result reflects one account's observed model mix and is not a guaranteed plan allowance. A snapshot newer than the ledger waits for another ingest without hiding an older, caught-up estimate.

`cswitch remove` retains billing settings; `--purge-usage` deletes them with ledger rows and limit history.

Without configuration, attribution uses the nearest Git root. A private `~/.claude-switch/usage/config.json` can add project folders, workspace names, and aliases. This synthetic example uses only placeholder paths:

```json
{
  "superproject": "/srv/example/atlas",
  "project_globs": ["teams/*/apps/*", "teams/*/sites/*"],
  "workspaces": [
    { "glob": "teams/*", "segment": 1 },
    { "glob": "notes", "name": "notes" }
  ],
  "ignore_paths": ["/srv/example/scratch"],
  "aliases": { "blue/old-ui": "blue/site" }
}
```

Nested Git repositories take priority over folder globs. Explicit labels and `/rename` titles take priority over request `cwd`; then file paths can attribute requests whose session has at least 60% of its file touches in one project. `--explain` shows the chosen signal. The report footer gives the earliest ingested timestamp, since deleted transcripts cannot be recovered from the ledger. Absolute `ignore_paths` prefixes remove matching working directories and file touches from attribution and its 60% denominator.

`config.json` applies when a row is ingested, so re-attributing old rows means deleting the ledger and re-ingesting while the transcripts still exist.

## Interactive TUI

Run `cswitch` with no arguments to open the TUI.

The profile list places each Claude profile's `30D $` value on its name line. The detail panel shows the same usage, rate or plan, effective-rate, and capacity lines as `cswitch info`, below the plan limits. The TUI ingests transcript usage in the background when it opens and refreshes the display when ingestion finishes. A busy ledger or ingest error appears in the panel; billing settings remain CLI commands.

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
| `Enter` | Launch the selected profile's tool |
| `/` | Search profiles by name or email |
| `a` | Add account — enter a name, then choose Claude Copy/login, `[o]` Codex login or `[g]` Antigravity login |
| `l` | Login — shortcut straight to a different account |
| `p` | Enter a masked API key for a Claude profile |
| `P` | Clear a Claude profile's API key after confirmation |
| `r` | Refresh a Claude profile from the current session (confirmed) |
| `d` | Delete selected profile (confirmed) |
| `?` | Help overlay |
| `q` / `Esc` | Quit |

## Copy vs Login

A profile is a **config environment**, not an identity. Its name is a local label you choose; the Claude account inside it comes from authentication and nothing else. The `a` menu offers copy, subscription login, and Console login.

**Copy current session** — same Claude account, separate setup.

```bash
cswitch add review      # → choose [c]
```

Use this for one account with two environments: different MCP servers, different project trust, separate conversation history. `review` and your main profile stay the same account.

**Login to a different Claude account** — a different identity.

```bash
cswitch login business   # or press `a`, then [l]
```

The new profile is seeded with your warm setup (settings, linked user-level skills, project trust), then every trace of the old account is stripped so Claude has to authenticate from scratch.

### What carries over — and what doesn't

A profile is a copy of your **config directory**. That boundary decides everything:

| | Carries over |
|---|---|
| Settings (`settings.json`) | Yes |
| **User-level** skills — `~/.claude/skills/` | Yes, linked one by one |
| Project trust and onboarding state | Yes |
| MCP server **definitions** | Yes |
| **Project-level** skills — `<your-repo>/.claude/skills/` | **No** — they belong to the repo, not the config directory |
| MCP **authorizations** (OAuth) | **No** — a grant belongs to the account that gave it |
| Conversation history and transcripts | No, unless you pass `--include-history` |

Each user-level skill in `~/.claude/skills/` is linked through its source entry. A skill linked from that directory into a repository remains reachable from every profile and keeps tracking edits in the repository.

## Skills sync

`~/.claude/skills/` is the read-only source for shared user-level skills. `cswitch sync --all` adds an absolute link for each eligible skill in each registered profile. Run `cswitch sync personal --dry-run` to preview changes. `cswitch use personal` also syncs before launching Claude; sync errors produce a warning and do not stop the launch. Arguments after the profile name, including `--resume`, `-p`, `--model`, `--help`, and `--`, pass through to Claude.

Sync skips dotfiles and `synced/`. Each profile keeps its own `skills/synced/`; sync never reads or changes it. Existing links to the matching source entry stay in place. Links elsewhere stay in place and are reported as foreign. A real file or directory whose full tree is byte-identical to the source is moved to `~/.claude-switch/backups/skills/<profile>/<skill>-<UTC timestamp>` and replaced with a link. A differing copy is reported as diverged and kept; `cswitch sync personal --adopt <skill>` backs it up and links the source instead. Profile-only entries are kept, except dangling links into the source directory, which are removed when that source directory exists. A missing source directory causes no changes.

Two consequences worth expecting:

- **A server like Whimsical will ask you to authenticate again.** The server definition copied fine; the new account has simply never authorised it. That is correct behavior, not lost configuration.
- **Skills that live in a repo stay with that repo.** `CLAUDE_CONFIG_DIR` does not relocate a project's `.claude/skills/`, so those load from wherever you launch Claude, identically under every profile. If a skill seems to have gone missing after a switch, check which of the two kinds it is before suspecting the profile.

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

After that, `a` in the TUI also offers Anthropic Console login for every additional profile.

## Shell aliases

Generate aliases so you can launch profiles through `cswitch use`:

```bash
cswitch aliases >> ~/.zshrc   # or ~/.bashrc
source ~/.zshrc
```

For each Claude or Codex profile, `cswitch aliases` emits one alias named after its tool:
`claude-<name>` or `codex-<name>`. This gives you commands like:

```bash
claude-work --resume       # syncs skills, then launches Claude with the "work" profile
claude-personal --model opus
codex-research              # launches Codex with the "research" profile
```

On Windows, `cswitch aliases` outputs PowerShell functions instead. Add them to your `$PROFILE`.

## Platform support

| | macOS | Linux | Windows |
|---|---|---|---|
| Profile management | Yes | Yes | Yes |
| Credential handling | Keychain | File-based | Credential Manager |
| Shell aliases | bash/zsh | bash/zsh | PowerShell |
| TUI | Yes | Yes | Yes |
| Antigravity profiles | Untested | Yes | No |

## How profiles are stored

Profiles live in `~/.claude-switch/profiles/<name>/`. Claude profiles are Claude Code config directories; Codex profiles are `CODEX_HOME` directories; Antigravity profiles contain an isolated `home/`. `cswitch use <name>` launches the selected tool with its corresponding directory.

Nothing in your original `~/.claude` is modified. Profiles are fully isolated from each other.

## Running multiple accounts simultaneously

Switching is not the only mode — profiles run **at the same time**. `CLAUDE_CONFIG_DIR` is read per process, so each terminal is bound to whichever profile launched it, for as long as it lives:

```bash
# Terminal 1
cswitch use work

# Terminal 2
cswitch use personal
```

Both run independently with their own credentials, settings, and MCP servers. You can keep a work session and a personal session open side by side, in the same repository, indefinitely. Shell aliases (`cswitch aliases`) work the same way.

### One thing to watch

The profile list is shared, but a running session is not: `r` (refresh) and `d` (delete) act on files another terminal may have open right now. Refreshing a profile mid-session replaces its credentials underneath a live login; deleting one removes the config directory that session is reading from.

`cswitch` checks for this. When a profile was written to recently, the confirmation turns red and says so:

```
  In use? Written 4 min ago by a Claude session.
  Another terminal may have this profile open right now.
```

It is a warning, not a block — `cswitch` can see that a session wrote to the profile, not whether that terminal is still open. If you know it is closed, go ahead.

## License

MIT
