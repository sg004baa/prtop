# prtop

A terminal-resident TUI that monitors GitHub pull requests you're involved in as author or reviewer, updating in real time via periodic polling.

## Features

- Lists open PRs where you are the author or a requested reviewer
- Auto-refreshes on a configurable interval
- Terminal notifications on key events (merged, review requested, re-review requested)
- Keyboard navigation with browser open on Enter
- Compact inline display — fits alongside other terminal panes

## Platform Support

Tested on Linux(Ubuntu). macOS and Windows are untested.

## Installation

```bash
cargo install prtop
prt
```

## Configuration

Create `~/.config/prtop/config.toml`:

```toml
github_token = "ghp_xxx"
username = "github-username"
poll_interval_secs = 60  # optional, default: 60

[notify]
enabled = false  # set to true to enable OSC 9 notifications
```

See `config.example.toml` for a full example.

Authentication can also be provided via CLI flags or environment variables

| Setting | Flag | Env var |
|---|---|---|
| GitHub token | `--github-token` | `PRTOP_GITHUB_TOKEN` |
| Username | `--username` | `PRTOP_GITHUB_USERNAME` |


> [!CAUTION]
> Grant **read-only** permissions only. prtop never writes to GitHub

Required scopes (fine-grained token): **Pull requests: Read-only**, **Metadata: Read-only**

## Notifications

Notifications are sent via the OSC 9 escape sequence — this requires a terminal with OSC 9 support, such as WezTerm.

To enable, add to `config.toml`:

```toml
[notify]
enabled = true
```

Events that trigger a notification:

| Event | Condition |
|---|---|
| PR closed/merged | You are the **author** |
| Review requested | You are **not** the author |
| Re-review requested | `review_decision` changed to `ReviewRequired` |


## Color Scheme

All UI colors can be customized in `config.toml`:

```toml
[colors]
app_title    = "cyan"      # "GitHub PR Live" in header
col_header   = "dark_gray" # column header row
role         = "cyan"      # AUTHOR / REVIEW / BOTH
number       = "yellow"    # #1234
repo         = "blue"      # org/repo
new_pr       = "green"     # newly appeared PRs
# draft      = "#888888"   # hex also accepted
footer_count = "green"     # "3 PRs" in footer
```

Accepted values: `#rrggbb` hex, or named colors (`black`, `red`, `cyan`, `dark_gray`, etc.). Unknown values fall back to the default.

## Keybindings

| Key | Action |
|---|---|
| `j` / `↓` | Move down |
| `k` / `↑` | Move up |
| `Enter` / `o` | Open PR in browser |
| `r` | Force refresh |
| `?` | Toggle help |
| `q` / `Ctrl+C` | Quit |

