# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                    # Dev build
cargo build --release          # Release build
cargo fmt                      # Format code
cargo fmt --check              # Check formatting (CI uses this)
cargo clippy -- -D warnings    # Lint (CI uses this)
cargo test                     # Run all tests
cargo test diff::tests         # Run tests in a specific module
cargo test --test diff_logic   # Run a specific integration test file
cargo test test_name           # Run a single test by name
cargo license                  # List dependency licenses (requires: cargo install cargo-license)
```

## Pre-push Checklist

Before pushing, always run:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

## Architecture

Terminal-resident TUI app that monitors GitHub PRs you're involved in (as author, reviewer, or mentioned user), using The Elm Architecture (TEA) pattern.

### Async Task Model

Three tokio tasks communicate via bounded mpsc channels (capacity 64) into a single main loop:

1. **Event Task** (`tui/event.rs`) — reads crossterm keyboard events, maps to `Message` variants
2. **Poller Task** (`poller.rs`) — polls GitHub GraphQL API on interval, sends `PollResult`/`PollError`
3. **Main Loop** (`main.rs`) — owns `App` state and terminal, uses `tokio::select!` with 200ms timeout

The main loop only redraws when `app.dirty` is true. `CancellationToken` coordinates graceful shutdown across all tasks.

### TEA Pattern (app.rs)

`App` is the model, `Message` is the message enum, `App::update()` is the update function, and `tui/ui.rs::view()` is the view function. All state mutations go through `update()`.

### GitHub API (github/)

Three GraphQL search queries (`author:{user}`, `review-requested:{user}`, `mentions:{user}`), each with an open and a closed variant (six searches total), are executed in parallel via `tokio::join!`, then merged in `poller::merge_and_convert()`. A PR appearing in multiple queries resolves its role by priority `Author > ReviewRequested > Mentioned`. Pagination follows `endCursor` up to 4 pages (200 items max).

### Mention Dismissals (dismiss.rs)

Mentioned-role PRs are dismissed when opened in the browser (`Message::OpenSelected`): `App.pending_dismissals` is drained by the main loop into `DismissStore`, persisted at `<cache_dir>/prtop/dismissed.json` (`{"owner/repo#123": "<RFC3339>"}`). The poller hides still-dismissed mentioned PRs, prunes entries no longer returned by the mentions queries, and un-dismisses a PR when a newer issue comment by someone else contains `@username` (checked via `fetch_recent_comments`, an alias-batched GraphQL query; review-thread comments are out of scope). The store is shared as `Arc<std::sync::Mutex<DismissStore>>`; locks are never held across `.await`.

### Error Classification (error.rs)

`AppError` variants drive retry behavior: `RateLimited` waits for `Retry-After`, `Transient` uses exponential backoff (2s→60s), `Auth` stops polling permanently.

### Notification System (notify.rs)

`Notifier` trait with four backends: `OscNotifier` (OSC 9 escape to stderr), `BellNotifier` (BEL to stderr), `ExecNotifier` (shell command), `NullNotifier` (default, noop). Initialized in `main.rs` from config; `App` accumulates `pending_notifications: Vec<Notification>` which the main loop drains and sends via the notifier after each `update()`.

Notification triggers (only after initial load, checked in `Message::PollResult` handler):
- `removed` + `role == Author` → "PR closed/merged"
- `added` + `role == ReviewRequested` → "Review requested"
- `added` + `role == Mentioned` → "Mentioned in PR"
- `updated` + `review_decision` changed to `ReviewRequired` → "Re-review requested"

### Key Data Structures

- `IndexMap<PrId, PullRequest>` — single source of truth for PR list (ordered + O(1) lookup)
- `diff.rs::diff_pr_sets()` — computes added/removed/updated between poll cycles
- `App.new_pr_ids: HashSet<PrId>` — tracks newly added PRs for green highlight; cleared per-PR when user navigates to it

### Config Priority

CLI args > env vars (`PRTOP_GITHUB_TOKEN`, `PRTOP_GITHUB_USERNAME`) > `~/.config/prtop/config.toml`

Config keys: `github_token`, `username`, `poll_interval_secs`, `[notify].enabled`, plus per-event toggles under `[notify]`.

Per-event toggles: `review_requested`, `mentioned`, `pr_closed`, `pr_merged`, `re_review_requested`, `new_comment`, `ci_finished`. All default `true` except `ci_finished` (default `false`). Omit a key to use its default. `enabled = false` is a global kill switch (defaults to `false`). `ci_finished` only controls the notification — CI status is always fetched per poll to populate the `CI` column. Token needs `commit_statuses: read` and/or `checks: read` to actually see CI state; without those scopes the calls 403/404 silently.
