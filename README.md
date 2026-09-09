# prtop

A terminal-resident TUI that monitors GitHub pull requests you're involved in as author, reviewer, or mentioned user, updating in real time via periodic polling.

![](<スクリーンショット 2026-03-21 144858.png>)

## Features

- Lists PRs where you are the author, a requested reviewer, or mentioned, with status (Open/Closed/Merged)
- Auto-refreshes on a configurable interval
- Terminal notifications on key events (merged, review requested, re-review requested, mentioned)
- Role-grouped keyboard navigation; Enter toggles groups or opens a PR
- Mentioned PRs disappear once opened and come back only when you are mentioned again
- Compact inline display — fits alongside other terminal panes

## Platform Support

Tested on Linux(Ubuntu). macOS and Windows are untested.

## Installation
### Homebrew
```bash
brew install sg004baa/tap/prtop
```

### Cargo
```bash
cargo install prtop
```

## Usage
```bash
prt
```

## Configuration

Create `~/.config/prtop/config.toml`:

```toml
github_tokens = ["ghp_xxx", "github_pat_yyy"]
username = "github-username"
poll_interval_secs = 60  # optional, default: 60
```

See `config.example.toml` for a full example.

Fine-grained GitHub PATs are limited to one resource owner. Configure one
token per organization or other resource owner whose PRs you want to monitor.
PRs visible through multiple tokens are deduplicated and merged into one entry.
If a token fails, its most recent successful PR set is retained while the
other tokens continue updating. Each failed token emits an error identified as
`GitHub token #N`, rather than an organization name.

Authentication is configured with the `github_tokens` array above. The username
can also be supplied via CLI or environment variable:

| Setting  | Flag          | Env var                 |
| -------- | ------------- | ----------------------- |
| Username | `--username`  | `PRTOP_GITHUB_USERNAME` |

> [!CAUTION]
> Grant **read-only** permissions to every token. prtop never writes to GitHub.

Every token needs these fine-grained permissions: **Pull requests: Read-only**
and **Metadata: Read-only**. To populate CI status for PRs visible through a
token, that same token also needs **Commit statuses: Read-only** and/or
**Checks: Read-only**; without those permissions, CI calls silently return
403/404 and the `CI` column shows `-` for those PRs.

## Notifications

Notifications are sent via the OSC 9 escape sequence — this requires a terminal with OSC 9 support, such as WezTerm.

To enable, add to `config.toml`:

```toml
[notify]
enabled = true
# Per-event toggles (omit any line to use its default)
# review_requested    = true
# mentioned           = true
# pr_closed           = true
# pr_merged           = true
# re_review_requested = true
# new_comment         = true
# ci_finished         = false
```

`enabled` is a global kill switch — defaults to `false`, so notifications stay off until you opt in. Each event has its own toggle; omit a line to fall back to the default below.

| Event                 | Default | Condition                                                       |
| --------------------- | :-----: | --------------------------------------------------------------- |
| `review_requested`    |    ✅    | A new PR appears where review is requested from you             |
| `mentioned`           |    ✅    | A new PR appears where you are mentioned                        |
| `pr_closed`           |    ✅    | Your authored PR transitions to closed                          |
| `pr_merged`           |    ✅    | Your authored PR is merged                                      |
| `re_review_requested` |    ✅    | `review_decision` changes to `ReviewRequired`                   |
| `new_comment`         |    ✅    | Comment count increases on your authored PR (self-comment skip) |
| `ci_finished`         |    ❌    | CI transitions from in-progress to success/failure (author only) |

CI status is fetched every poll (per-PR REST calls to
`/repos/{owner}/{repo}/commits/{sha}/status` and `/check-runs`) and shown in
the `CI` column regardless of `ci_finished`. Each token needs the additional
**Commit statuses: Read-only** and/or **Checks: Read-only** permissions;
without them the calls return 403/404 silently and affected PRs show `-`.

On startup and refresh, the PR list appears before CI fetching completes. The
CI column updates afterward without moving your selection. While refreshing,
the previous CI status is kept only when the PR still has the same head commit;
new or changed commits show `-` until their CI status arrives.

`ci_finished` controls only whether a *notification* fires when CI
transitions from in-progress to success/failure. It defaults off because CI
flapping can be noisy.

## Role Groups

PRs are grouped as `AUTHOR`, `REVIEW`, and `MENTION`. Within each group,
repositories and PR numbers are sorted descending. Enter toggles a group, and
the collapsed state is persisted in `<config_dir>/prtop/ui-state.json`.

## Mentions

PRs where you are mentioned (GitHub search `mentions:{username}`) show up with the
`MENTION` role. Opening a mentioned PR in the browser dismisses it from the list;
it reappears (with a notification) only when someone else mentions you again in an
issue comment on that PR. Dismissals are persisted in
`<cache_dir>/prtop/dismissed.json` (e.g. `~/.cache/prtop/dismissed.json`).

## Color Scheme

All UI colors can be customized in `config.toml`:

```toml
[colors]
app_title    = "cyan"         # "GitHub PR Live" in header
col_header   = "dark_gray"    # column header row
role         = "cyan"         # AUTHOR / REVIEW / MENTION
number       = "yellow"       # #1234
repo         = "blue"         # repository name
new_pr       = "green"        # newly appeared PRs
new_comment  = "light_yellow" # PRs with new comments
draft        = "dark_gray"    # draft PRs
footer_count = "green"        # "3 PRs" in footer
# app_title  = "#00bfff"      # hex also accepted
```

Accepted values: `#rrggbb` hex, or named colors (`black`, `red`, `cyan`, `dark_gray`, etc.). Unknown values fall back to the default.

## Keybindings

| Key            | Action                              |
| -------------- | ----------------------------------- |
| `j` / `↓`      | Move down                           |
| `k` / `↑`      | Move up                             |
| `Enter`        | Toggle selected role / open PR      |
| `o`            | Open selected PR in browser         |
| `r`            | Force refresh                       |
| `?`            | Toggle help                         |
| `q` / `Ctrl+C` | Quit                                |
