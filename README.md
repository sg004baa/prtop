# prtop

A terminal-resident TUI that monitors GitHub pull requests you're involved in as author, reviewer, or mentioned user, updating in real time via periodic polling.

![](<スクリーンショット 2026-03-21 144858.png>)

## Features

- Lists PRs where you are the author, a requested reviewer, or mentioned, with status (Open/Closed/Merged)
- Auto-refreshes on a configurable interval
- Terminal notifications on key events (merged, review requested, re-review requested, mentioned, all reviewers approved)
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
and **Metadata: Read-only**. CI visibility also depends on access to commit
statuses and checks; grant **Commit statuses: Read-only** and **Checks: Read-only**
for the repositories being monitored. GraphQL permission errors are reported
rather than silently falling back to REST.

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
# pr_approved         = true
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
| `pr_approved`         |    ✅    | All tracked reviewers approve your open, non-draft PR           |
| `ci_finished`         |    ❌    | CI transitions from in-progress to success/failure (author only) |

CI status comes from the head commit's GraphQL `statusCheckRollup`, included in
normal PR searches. There are no per-PR REST status/check-runs requests and no
separate CI polling timer. Refreshing searches observes CI changes even when the
head SHA and PR timestamp have not changed (including workflow reruns).

The `CI` column follows GitHub's aggregate state: `SUCCESS` is success,
`FAILURE`/`ERROR` are failure, and `PENDING`/`EXPECTED` are in progress. Missing
rollups, empty contexts, unknown states, or a mismatched head commit show `-`.
This uses GitHub's aggregation rules instead of combining individual checks
locally; mixed check outcomes and action-required checks may differ from the
previous REST-based display. CI updates use the existing notification path.
GraphQL query cost increases, so GraphQL rate limits can still require waiting.

`ci_finished` controls only whether a *notification* fires when CI
transitions from in-progress to success/failure. It defaults off because CI
flapping can be noisy.

## Review progress

The `REVIEW` column shows `approved 1/2`: approvals out of unique people who
have submitted a review or currently have a review request. The PR author and
unsubmitted (pending) reviews are excluded. Below 100 terminal columns the
label is shortened to `1/2`. A dash (`—`) means no reviewers; `?` means loading
or an unavailable/incomplete review fetch.

- A person's latest approval or changes-requested decision counts. A later
  comment does not revoke that decision; a dismissed review does.
- An outstanding review request makes that person unapproved, including a
  re-review request after approval. People who only commented count as unapproved.
- `!` marks a changes-requested decision. Green means everyone approved.
- Outstanding team requests show `+ team pending` (`+team` on narrow terminals)
  and block completion. Team membership is not expanded into the denominator.
  Once the team request resolves, the individuals who reviewed are counted.
- `0/0` never counts as approval. This is independent of GitHub's required
  approval count / `reviewDecision` and does not imply that the PR is mergeable.

Reviews and review requests are fetched asynchronously for open PRs, with
pagination and the existing **Pull requests: Read-only** permission. Unchanged
review summaries are cached for five minutes; a changed PR timestamp, review
decision or head commit triggers an earlier check. Periodic rechecks also detect
review changes that do not update the PR timestamp. Failed fetches retry no
sooner than one minute later. Closed/merged search history is not enriched.

Counts are published only after both connections are complete. A failed fetch
keeps the previous count with `(stale)` (`~` on narrow terminals), in gray; `?`
is used only when no complete count is available. Stale counts cannot generate
approval notifications. A new head commit also marks the old count stale until
its review fetch succeeds.

Requests are serialized per token. Primary rate-limit cooldowns are tracked
separately for GraphQL and REST (`core`). PR searches and CI now both use
GraphQL, so a GraphQL cooldown pauses updates to both; REST quota is not consumed
for CI.
Secondary limits pause both resources. Requests queued during a cooldown are
rejected locally without contacting GitHub. The client respects `Retry-After`
and `x-ratelimit-reset`, with a 60-second fallback if the deadline is missing.
A `503` with `Retry-After` is a temporary service failure, not a rate-limit error.

Tokens belonging to the same GitHub user share primary quotas. prtop learns
`viewer.id` from normal search responses (without an additional request) and
shares observed cooldowns between tokens with the same verified user ID and API
host. Tokens whose identity is not yet available remain separate, and different
users are never grouped based on the configured username. State is in memory;
restarting prtop does not reset GitHub's quota.

Rate-limit diagnostics show the HTTP status, endpoint path, resource, remaining
quota and UTC reset timestamp, or `unknown` for missing headers. Cached cooldown
errors retain the original response evidence. Press `?` to inspect the last API
error in the help screen, even after a subsequent successful poll clears the
footer warning. Credentials and request bodies
are excluded from this diagnostic. An actual exhausted primary quota still needs
to wait for its reset; a long countdown alone does not prove the current process
consumed that quota.

On the initial successful load, only open-PR searches are performed (three
searches per token before pagination, rather than six). Closed/merged searches
resume on later polls to detect transitions of tracked PRs. CI rollups refresh
with these searches, including PRs without configured CI.

`pr_approved` sends `PR approved` only when a previously observed incomplete
review state becomes fully approved on your open, non-draft PR. The first
successful observation is silent (including after startup or a PR first appears).
Repeated complete states and recovery from a failed fetch do not repeat an
already sent notification. If an incomplete state is observed and all reviewers
approve again, it notifies again. Changes occurring entirely between polls cannot
be detected. The event defaults on, subject to `[notify].enabled` (default off).

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
