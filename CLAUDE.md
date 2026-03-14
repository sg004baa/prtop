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

Terminal-resident TUI app that monitors GitHub PRs you're involved in (as author or reviewer), using The Elm Architecture (TEA) pattern.

### Async Task Model

Three tokio tasks communicate via bounded mpsc channels (capacity 64) into a single main loop:

1. **Event Task** (`tui/event.rs`) — reads crossterm keyboard events, maps to `Message` variants
2. **Poller Task** (`poller.rs`) — polls GitHub GraphQL API on interval, sends `PollResult`/`PollError`
3. **Main Loop** (`main.rs`) — owns `App` state and terminal, uses `tokio::select!` with 200ms timeout

The main loop only redraws when `app.dirty` is true. `CancellationToken` coordinates graceful shutdown across all tasks.

### TEA Pattern (app.rs)

`App` is the model, `Message` is the message enum, `App::update()` is the update function, and `tui/ui.rs::view()` is the view function. All state mutations go through `update()`.

### GitHub API (github/)

Two GraphQL search queries (`author:{user}` and `review-requested:{user}`) are executed in parallel via `tokio::join!`, then merged in `poller::merge_and_convert()`. Same PR appearing in both queries gets `PrRole::Both`. Pagination follows `endCursor` up to 4 pages (200 items max).

### Error Classification (error.rs)

`AppError` variants drive retry behavior: `RateLimited` waits for `Retry-After`, `Transient` uses exponential backoff (2s→60s), `Auth` stops polling permanently.

### Notification System (notify.rs)

`Notifier` trait with four backends: `OscNotifier` (OSC 9 escape to stderr), `BellNotifier` (BEL to stderr), `ExecNotifier` (shell command), `NullNotifier` (default, noop). Initialized in `main.rs` from config; `App` accumulates `pending_notifications: Vec<Notification>` which the main loop drains and sends via the notifier after each `update()`.

Notification triggers (only after initial load, checked in `Message::PollResult` handler):
- `removed` + `role == Author` → "PR closed/merged"
- `added` + `role != Author` → "Review requested"
- `updated` + `review_decision` changed to `ReviewRequired` → "Re-review requested"

### Key Data Structures

- `IndexMap<PrId, PullRequest>` — single source of truth for PR list (ordered + O(1) lookup)
- `diff.rs::diff_pr_sets()` — computes added/removed/updated between poll cycles
- `App.new_pr_ids: HashSet<PrId>` — tracks newly added PRs for green highlight; cleared per-PR when user navigates to it

### Config Priority

CLI args > env vars (`GITHUB_TOKEN`, `GITHUB_USERNAME`) > `~/.config/prtop/config.toml`

Config keys: `github_token`, `username`, `poll_interval_secs`, `[notify].backend`, `[notify].exec_command`
